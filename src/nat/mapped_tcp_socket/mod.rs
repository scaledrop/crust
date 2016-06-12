// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::collections::HashSet;
use std::any::Any;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;

use igd::PortMappingProtocol;
use net2::TcpBuilder;
use mio::tcp::TcpStream;
use mio::{EventLoop, Timeout, Token};

use core::{Context, Core, CoreMessage};
use core::state::State;
use nat::{MappedAddr, MappingContext, NatError, util};
use self::get_ext_addr::GetExtAddr;

mod get_ext_addr;

const TIMEOUT_SECS: u64 = 60;

/// A state which represents the in-progress mapping of a tcp socket.
pub struct MappingTcpSocket<F> {
    token: Token,
    context: Context,
    socket: Option<TcpBuilder>,
    igd_children: usize,
    stun_children: HashSet<Context>,
    mapped_addrs: Vec<MappedAddr>,
    timeout: Timeout,
    finish: Option<F>,
}

impl<F> MappingTcpSocket<F>
    where F: FnOnce(&mut Core, &mut EventLoop<Core>, TcpBuilder, Vec<MappedAddr>) + Any
{
    /// Start mapping a tcp socket
    pub fn start(core: &mut Core,
                 event_loop: &mut EventLoop<Core>,
                 port: u16,
                 mc: &MappingContext,
                 finish: F)
                 -> Result<(), NatError> {
        let token = core.get_new_token();
        let context = core.get_new_context();

        // TODO(Spandan) Ipv6 is not supported in Listener so dealing only with ipv4 right now
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);

        let socket = try!(util::new_reusably_bound_tcp_socket(&addr));
        let addr = try!(util::tcp_builder_local_addr(&socket));

        // Ask IGD
        let mut igd_children = 0;
        for &(ref ip, ref gateway) in mc.ifv4s() {
            let tx = event_loop.channel();
            let gateway = match *gateway {
                Some(ref gateway) => gateway.clone(),
                None => continue,
            };
            let addr_igd = SocketAddrV4::new(*ip, addr.port());
            let _ = thread!("IGD-Address-Mapping", move || {
                let res =
                    gateway.get_any_address(PortMappingProtocol::TCP, addr_igd, 0, "MaidSafeNat");
                let ext_addr = match res {
                    Ok(ext_addr) => ext_addr,
                    Err(_) => return,
                };
                let _ = tx.send(CoreMessage::new(move |core, el| {
                    let state = match core.get_state(context) {
                        Some(state) => state,
                        None => return,
                    };

                    let mut state = state.borrow_mut();
                    let mapping_tcp_sock = match state.as_any()
                        .downcast_mut::<MappingTcpSocket<F>>() {
                        Some(mapping_sock) => mapping_sock,
                        None => return,
                    };
                    mapping_tcp_sock.handle_igd_resp(core, el, SocketAddr::V4(ext_addr));
                }));
            });
            igd_children += 1;
        }

        let mapped_addrs = mc.ifv4s()
            .iter()
            .map(|&(ip, _)| MappedAddr::new(SocketAddr::new(IpAddr::V4(ip), addr.port()), false))
            .collect();

        let state = Rc::new(RefCell::new(MappingTcpSocket {
            token: token,
            context: context,
            socket: Some(socket),
            igd_children: igd_children,
            stun_children: HashSet::with_capacity(mc.peer_listeners().len()),
            mapped_addrs: mapped_addrs,
            timeout: try!(event_loop.timeout(token, Duration::from_secs(TIMEOUT_SECS))),
            finish: Some(finish),
        }));

        // Ask Stuns
        for peer_stun in mc.peer_listeners() {
            let query_socket = try!(util::new_reusably_bound_tcp_socket(&addr));
            let query_socket = try!(query_socket.to_tcp_stream());
            let socket = try!(TcpStream::connect_stream(query_socket, &peer_stun));

            let self_weak = Rc::downgrade(&state);
            let handler = move |core: &mut Core, el: &mut EventLoop<Core>, child_context, res| {
                if let Some(self_rc) = self_weak.upgrade() {
                    self_rc.borrow_mut().handle_stun_resp(core, el, child_context, res)
                }
            };

            if let Ok(child) = GetExtAddr::start(core, event_loop, socket, Box::new(handler)) {
                let _ = state.borrow_mut().stun_children.insert(child);
            }
        }

        let _ = core.insert_context(token, context);
        let _ = core.insert_state(context, state);

        Ok(())
    }

    fn handle_stun_resp(&mut self,
                        core: &mut Core,
                        event_loop: &mut EventLoop<Core>,
                        child: Context,
                        res: Result<SocketAddr, ()>) {
        let _ = self.stun_children.remove(&child);
        if let Ok(our_ext_addr) = res {
            self.mapped_addrs.push(MappedAddr::new(our_ext_addr, true));
        }
        if self.stun_children.is_empty() && self.igd_children == 0 {
            let _ = self.terminate(core, event_loop);
        }
    }

    fn handle_igd_resp(&mut self,
                       core: &mut Core,
                       event_loop: &mut EventLoop<Core>,
                       our_ext_addr: SocketAddr) {
        self.igd_children -= 1;
        self.mapped_addrs.push(MappedAddr::new(our_ext_addr, false));
        if self.stun_children.is_empty() && self.igd_children == 0 {
            let _ = self.terminate(core, event_loop);
        }
    }

    fn terminate_children(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>) {
        for context in self.stun_children.drain() {
            let child = match core.get_state(context) {
                Some(state) => state,
                None => continue,
            };

            child.borrow_mut().terminate(core, event_loop);
        }
    }
}

impl<F> State for MappingTcpSocket<F>
    where F: FnOnce(&mut Core, &mut EventLoop<Core>, TcpBuilder, Vec<MappedAddr>) + Any
{
    fn timeout(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>, _: Token) {
        return self.terminate(core, event_loop);
    }

    fn terminate(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>) {
        self.terminate_children(core, event_loop);
        let _ = core.remove_context(self.token);
        let _ = core.remove_state(self.context);
        let _ = event_loop.clear_timeout(&self.timeout);

        let socket = self.socket.take().expect("Logic Error");
        let mapped_addrs = self.mapped_addrs.drain(..).collect();
        (self.finish.take().unwrap())(core, event_loop, socket, mapped_addrs);
    }

    fn as_any(&mut self) -> &mut Any {
        self
    }
}
