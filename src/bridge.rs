/*
 * Copyright 2015-2016 Torrie Fischer <tdfischer@hackerbots.net>
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use irc;
use matrix;
use irc::protocol::{Command,Message};
use irc::streams::AsEvented;
use mio;
use mio::{EventLoop,Handler,Token,EventSet,PollOpt,Sender};
use std::thread;
use std::collections::HashMap;
use std::io;

const CLIENT: Token = Token(0);

#[derive(Debug)]
pub enum Event {
    EndPoll,
    Matrix(matrix::events::Event)
}

pub struct Bridge {
    client: irc::streams::Client,
    matrix: matrix::client::Client,
    rooms: HashMap<matrix::model::RoomID, Room>,
    seen_events: Vec<matrix::model::EventID>,
}

impl Handler for Bridge {
    type Timeout = ();
    type Message = Event;

    fn ready(&mut self, event_loop: &mut EventLoop<Bridge>, token: Token, _: EventSet) {
        match token {
            CLIENT =>
                self.handle_client(event_loop),
            _ => unreachable!("Got a really weird Token in the mio event loop!")
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Bridge>, msg: Self::Message) {
        match msg {
            Event::EndPoll => {
                self.poll_matrix(event_loop.channel());
            },
            Event::Matrix(e) =>
                match self.handle_matrix(e) {
                    Err(err) => warn!("Could not handle matrix event: {:?}", err),
                    _ => ()
                }
        };
    }
}

unsafe impl Sync for Bridge{}

struct Room {
    id: matrix::model::RoomID,
    canonical_alias: Option<String>,
    join_rules: Option<String>,
    members: Vec<matrix::model::UserID>,
    pending_events: Vec<matrix::events::RoomEvent>
}

impl Room {
    fn new(id: matrix::model::RoomID) -> Self {
        Room {
            id: id,
            canonical_alias: None,
            join_rules: None,
            members: vec![],
            pending_events: vec![]
        }
    }

    fn run_pending<F>(&mut self, mut callback: &mut F)
            where F: FnMut(irc::protocol::Message) {
        while let Some(evt) = self.pending_events.pop() {
            self.handle_with_alias(evt, callback);
        }
    }

    fn handle_with_alias<F>(&mut self, evt: matrix::events::RoomEvent, mut callback: &mut F)
            where F: FnMut(irc::protocol::Message) {
        if self.canonical_alias != None {
            match evt {
                matrix::events::RoomEvent::Membership(user, matrix::events::MembershipAction::Join) => {
                    callback(irc::protocol::Message {
                        prefix: Some(format!("{}!{}@{}", user.nickname, user.nickname, user.homeserver)),
                        command: irc::protocol::Command::Join,
                        args: vec![self.canonical_alias.clone().unwrap()],
                        suffix: None
                    });
                    self.members.push(user);
                },
                matrix::events::RoomEvent::Membership(user, matrix::events::MembershipAction::Leave) => {
                    callback(irc::protocol::Message {
                        prefix: Some(format!("{}!{}@{}", user.nickname, user.nickname, user.homeserver)),
                        command: irc::protocol::Command::Part,
                        args: vec![self.canonical_alias.clone().unwrap()],
                        suffix: None
                    });
                    self.members.push(user);
                },
                matrix::events::RoomEvent::Membership(_, _) => (),
                matrix::events::RoomEvent::Message(user, text) => {
                    callback(irc::protocol::Message {
                        prefix: Some(format!("{}!{}@{}", user.nickname, user.nickname, user.homeserver)),
                        command: irc::protocol::Command::Privmsg,
                        args: vec![self.canonical_alias.clone().unwrap()],
                        suffix: Some(text)
                    });
                },
                matrix::events::RoomEvent::Topic(user, topic) => {
                    callback(irc::protocol::Message {
                        prefix: Some(format!("{}!{}@{}", user.nickname, user.nickname, user.homeserver)),
                        command: irc::protocol::Command::Topic,
                        args: vec![self.canonical_alias.clone().unwrap()],
                        suffix: Some(topic.clone())
                    });
                },
                matrix::events::RoomEvent::CanonicalAlias(_) => unreachable!("canonical_alias should have been handled already!"),
                _ => {
                    warn!("Unhandled event {:?}", evt)
                }
            }
        } else {
            self.pending_events.push(evt);
        }
    }

    fn handle_event<F>(&mut self, evt: matrix::events::RoomEvent, mut callback: F)
            where F: FnMut(irc::protocol::Message) {
        match evt {
            matrix::events::RoomEvent::CanonicalAlias(name) => {
                let was_empty = self.canonical_alias == None;
                self.canonical_alias = Some(name.clone());
                if was_empty {
                    self.run_pending(&mut callback);
                }
            },
            matrix::events::RoomEvent::JoinRules(rules) =>
                self.join_rules = Some(rules.clone()),
            matrix::events::RoomEvent::Create => (),
            matrix::events::RoomEvent::Aliases(aliases) => {
                let is_empty = self.canonical_alias == None;
                if is_empty {
                    self.canonical_alias = Some(aliases[0].clone());
                    self.run_pending(&mut callback);
                }
            },
            matrix::events::RoomEvent::PowerLevels => (),
            matrix::events::RoomEvent::HistoryVisibility(_) => (),
            matrix::events::RoomEvent::Name(_, _) => (),
            matrix::events::RoomEvent::Avatar(_, _) => (),
            matrix::events::RoomEvent::Unknown(unknown_type, json) => {
                warn!("Unknown room event {}", unknown_type);
                trace!("raw event: {:?}", json);
            }
            _ => self.handle_with_alias(evt, &mut callback)
        };
    }
}


impl Bridge {
    pub fn room_from_matrix(&mut self, id: &matrix::model::RoomID) -> &mut Room {
        if !self.rooms.contains_key(id) {
            self.rooms.insert(id.clone(), Room::new(id.clone()));
        }
        match self.rooms.get_mut(id) {
            Some(room) => room,
            None => unreachable!("Couldn't find the room that we just created")
        }
    }

    pub fn room_from_irc(&mut self, id: &String) -> Option<&mut Room> {
        let mut room_id: Option<matrix::model::RoomID> = None;
        for (_, r) in self.rooms.iter_mut() {
            if let Some(ref alias) = r.canonical_alias {
                if alias == id {
                    room_id = Some(r.id.clone())
                }
            }
        }
        match room_id {
            Some(id) => Some(self.room_from_matrix(&id)),
            None => None
        }
    }

    pub fn new(client: irc::streams::Client, url: &str) -> Self {
        Bridge {
            client: client,
            matrix: matrix::client::Client::new(url),
            rooms: HashMap::new(),
            seen_events: vec![]
        }
    }

    pub fn run(&mut self) {
        let mut events = EventLoop::new().unwrap();
        events.register(self.client.as_evented(), CLIENT, EventSet::all(), PollOpt::edge()).unwrap();
        events.run(self).unwrap();
    }

    fn handle_matrix(&mut self, evt: matrix::events::Event) -> io::Result<usize> {
        let duplicate = match evt.id {
            Some(ref id) =>
                self.seen_events.contains(id),
            _ => false
        };
        if !duplicate {
            let mut messages: Vec<irc::protocol::Message> = vec![];
            {
                let append_msg = |msg: irc::protocol::Message| {
                    messages.push(msg);
                };
                match evt.data {
                    matrix::events::EventData::Room(room_id, room_event) => {
                        self.room_from_matrix(&room_id).handle_event(room_event, append_msg);
                    },
                    matrix::events::EventData::Typing(_) => (),
                    _ => warn!("Unhandled {}", evt.data.type_str())
                }
            }
            match evt.id {
                Some(id) =>
                    self.seen_events.push(id),
                None => ()
            };
            let mut res: Option<io::Result<usize>> = None;
            for ref msg in messages {
                res = Some(match res {
                    None => self.client.send(msg),
                    Some(r) => r.and(self.client.send(msg))
                })
            }
            match res {
                None => Ok(0),
                Some(e) => e
            }
        } else {
            Ok(0)
        }
    }

    fn poll_matrix(&mut self, channel: mio::Sender<Event>) ->
        thread::JoinHandle<matrix::client::Result> {
        let poll = self.matrix.poll_async();
        thread::spawn(move|| {
            poll.send().and_then(|evts| {
                for evt in evts {
                    channel.send(Event::Matrix(evt)).unwrap();
                };
                channel.send(Event::EndPoll).unwrap();
                Ok(())
            })
        })
    }

    fn start_matrix(&mut self, channel: mio::Sender<Event>) ->
        matrix::client::Result {
        self.matrix.sync().and_then(|events| {
            for e in events {
                match self.handle_matrix(e) {
                    // FIXME: Return error
                    Err(err) => warn!("Could not handle matrix event: {:?}", err),
                    _ => ()
                }
            }
            self.poll_matrix(channel);
            Ok(())
        })
    }

    fn handle_client(&mut self, events: &mut EventLoop<Bridge>) {
        loop {
            match self.client.read_message() {
                None => return,
                Some(message) => {
                    match message.command {
                        Command::Pass => {
                            self.client.auth.set_password(message.args[0].clone())
                        }
                        Command::Nick => {
                            let nickname = match message.suffix {
                                None => message.args[0].clone(),
                                Some(n) => n
                            };
                            self.client.set_nickname(nickname)
                        },
                        Command::User => {
                            self.client.auth.set_username(message.args[0].clone());
                            let auth = self.client.auth.consume();
                            match (auth.username, auth.password) {
                                (Some(username), Some(password)) => {
                                    self.matrix.login(username.trim(), password.trim())
                                        .and_then(|_| {
                                            self.start_matrix(events.channel())
                                        })
                                        .and_then(|_| {
                                            self.client.welcome(username.trim()).unwrap();
                                            debug!("Logged in a user");
                                            Ok(())
                                        }).expect("Could not login!");
                                },
                                _ => panic!("Username and/or password missing, and anonymous access isn't built yet.")
                            };
                        },
                        Command::Join => {
                            self.client.join(&message.args[0]).expect("Could not send JOIN");
                        },
                        Command::Ping => {
                            self.client.pong().expect("Could not send PONG");
                        },
                        Command::Quit => {
                            // FIXME: Logout of matrix and exit thread
                            return;
                        },
                        Command::Privmsg => {
                            let room_id = match self.room_from_irc(&message.args[0]) {
                                None => return (),
                                Some(room) => room.id.clone()
                            };
                            let evt = {
                                let id = self.matrix.uid.clone().unwrap();
                                matrix::events::EventData::Room(
                                    room_id,
                                    matrix::events::RoomEvent::Message(
                                        id, message.suffix.unwrap()))
                            };
                            self.seen_events.push(self.matrix.send(evt).expect("Could not send event"));
                        },
                        _ =>
                            warn!("unhandled {:?}", message)
                    }
                }
            }
        }
    }
}

