use std::{
	cell::RefCell,
	io::{self, Read},
	net::{TcpListener, ToSocketAddrs},
	rc::Rc,
	sync::mpsc::{self, Receiver, Sender},
	thread::{self, JoinHandle},
	time::{Duration, SystemTime},
};

use crate::{
	lua_api::LuaApi,
	math::{Mat3, Vector3},
	protocol::{ByteWriter, MsgCommon, Opcode},
	user::{User, UserEvent},
	user_list::UserList,
};

enum BureauSignal {
	Close,
}

pub struct BureauOptions {
	pub max_players: i32,
	pub aura_radius: f32,
}

pub struct BureauHandle {
	handle: Option<JoinHandle<()>>,
	signaller: Sender<BureauSignal>,
}

impl BureauHandle {
	#[allow(dead_code)] // Remove when done implementing WLS.
	pub fn close(&mut self) {
		let _ = self.signaller.send(BureauSignal::Close);
	}

	pub fn join(mut self) -> thread::Result<()> {
		self.handle
			.take()
			.expect("Tried to join invalid bureau.")
			.join()
	}
}

pub struct Bureau {
	user_list: Rc<RefCell<UserList>>,
	listener: TcpListener,
	receiver: Receiver<BureauSignal>,
	options: BureauOptions,
	lua_api: LuaApi,
}

impl Bureau {
	/// Starts a new bureau and returns a special handle for its thread.
	pub fn new<A>(addr: A, options: BureauOptions) -> io::Result<BureauHandle>
	where
		A: ToSocketAddrs,
	{
		let listener = TcpListener::bind(addr)?;
		listener.set_nonblocking(true)?;

		let (signaller, receiver) = mpsc::channel::<BureauSignal>();

		let handle = thread::spawn(|| {
			let user_list = Rc::new(RefCell::new(UserList::new(options.max_players)));
			let lua_api = match LuaApi::new(user_list.clone()) {
				Ok(v) => v,
				Err(e) => panic!("Failed to create lua api. {}", e),
			};

			Bureau {
				user_list,
				listener,
				receiver,
				options,
				lua_api,
			}
			.run()
		});

		Ok(BureauHandle {
			handle: Some(handle),
			signaller,
		})
	}

	fn run(self) {
		let mut connecting = Vec::new();

		loop {
			if let Ok(signal) = self.receiver.try_recv() {
				match signal {
					BureauSignal::Close => break,
				}
			}

			let now = SystemTime::now();

			self.lua_api.think();

			if let Ok((socket, addr)) = self.listener.accept() {
				if self.lua_api.user_connecting(addr) {
					if let Ok(()) = socket.set_nonblocking(true) {
						connecting.push((now.clone(), socket));
					}
				}
			}

			// Handling pending users.
			let mut i = 0;
			while i < connecting.len() {
				let (connect_time, socket) = &mut connecting[i];

				let mut hello_buf = [0; 7];
				let n = match socket.read(&mut hello_buf) {
					Ok(n) => n,
					Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
						if let Ok(duration) = now.duration_since(*connect_time) {
							if duration.as_secs() > 10 {
								connecting.swap_remove(i);
							} else {
								i += 1;
							}
						} else {
							connecting.swap_remove(i);
						}

						continue;
					}
					Err(_) => {
						connecting.swap_remove(i);
						continue;
					}
				};

				let socket = connecting.swap_remove(i).1;

				if n < 7 {
					continue;
				}

				for j in 0..7 {
					// Last two bytes are vscp version.
					if hello_buf[j] != b"hello\x01\x01"[j] {
						continue;
					}
				}

				self.user_list.borrow_mut().new_user(socket);
			}

			// Handle connected users.
			for (_id, user) in self.user_list.borrow().users.iter() {
				match user.poll() {
					Some(events) => {
						for event in events {
							match event {
								UserEvent::NewUser(name, avatar) => {
									self.new_user(user, name, avatar)
								}
								UserEvent::StateChange => (),
								UserEvent::PositionUpdate(pos) => self.position_update(user, pos),
								UserEvent::TransformUpdate(mat, pos) => {
									self.transform_update(user, mat, pos)
								}
								UserEvent::ChatSend(msg) => self.chat_send(user, msg),
								UserEvent::CharacterUpdate(data) => {
									self.character_update(user, data)
								}
								UserEvent::NameChange(name) => self.name_change(user, name),
								UserEvent::AvatarChange(avatar) => self.avatar_change(user, avatar),
								UserEvent::PrivateChat(receiver, msg) => {
									self.private_chat(user, receiver, msg)
								}
								UserEvent::ApplSpecific(strategy, id, method, strarg, intarg) => {
									self.appl_specific(user, strategy, id, method, strarg, intarg)
								}
							}
						}
					}
					None => (),
				}

				if !user.is_connected() {
					self.disconnect_user(&user);
				}
			}

			let mut removed = false;
			self.user_list.borrow_mut().users.retain(|_, user| {
				let connected = user.is_connected();

				if !connected {
					self.lua_api.user_disconnect(user);
					removed = true;
				}

				connected
			});

			if removed {
				self.broadcast_user_count();
			}

			thread::sleep(Duration::from_millis(100));
		}
	}

	fn update_aura(&self, user: &User) {
		let user_pos = user.get_pos();

		for (id, other) in self.user_list.borrow().users.iter() {
			if *id == user.id {
				continue;
			}

			let dist = user_pos.get_distance_sqr(&other.get_pos());

			if user.check_aura(id) {
				if dist > self.options.aura_radius.powi(2) {
					user.remove_aura(&other.id);
					other.remove_aura(&user.id);

					// Tell other that user is gone
					other.send(&ByteWriter::general_message(
						other.id,
						other.id,
						Opcode::SMsgUserLeft,
						&ByteWriter::new().write_i32(user.id),
					));

					// Tell user that other is gone
					user.send(&ByteWriter::general_message(
						user.id,
						user.id,
						Opcode::SMsgUserLeft,
						&ByteWriter::new().write_i32(other.id),
					));

					self.lua_api.aura_leave(user, other);
				}
			} else if dist <= self.options.aura_radius.powi(2) {
				user.add_aura(other.id);
				other.add_aura(user.id);

				// Send user to other
				other.send(&ByteWriter::general_message(
					other.id,
					other.id,
					Opcode::SMsgUserJoined,
					&ByteWriter::new()
						.write_i32(user.id)
						.write_i32(user.id)
						.write_string(&user.get_avatar())
						.write_string(&user.get_name()),
				));
				other.send(&ByteWriter::message_common(
					other.id,
					other.id,
					user.id,
					MsgCommon::CharacterUpdate,
					1,
					&ByteWriter::new().write_string(&user.get_data()),
				));

				// Send other to user
				user.send(&ByteWriter::general_message(
					user.id,
					user.id,
					Opcode::SMsgUserJoined,
					&ByteWriter::new()
						.write_i32(other.id)
						.write_i32(other.id)
						.write_string(&other.get_avatar())
						.write_string(&other.get_name()),
				));
				user.send(&ByteWriter::message_common(
					user.id,
					user.id,
					other.id,
					MsgCommon::CharacterUpdate,
					1,
					&ByteWriter::new().write_string(&other.get_data()),
				));

				self.lua_api.aura_enter(user, other);
			}
		}
	}

	fn send_to_aura(&self, exluded: &User, stream: &ByteWriter) {
		let users = &self.user_list.borrow().users;
		for id in exluded.get_aura() {
			let user = match users.get(&id) {
				Some(u) => u,
				None => continue,
			};
			user.send(stream);
		}
	}

	fn send_to_aura_inclusive(&self, user: &User, stream: &ByteWriter) {
		let users = &self.user_list.borrow().users;
		user.send(stream);
		for id in user.get_aura() {
			let other = match users.get(&id) {
				Some(u) => u,
				None => continue,
			};
			other.send(stream);
		}
	}

	fn send_to_all(&self, stream: &ByteWriter) {
		for (_id, user) in self.user_list.borrow().users.iter() {
			user.send(stream);
		}
	}

	fn send_to_others(&self, user: &User, stream: &ByteWriter) {
		let users = &self.user_list.borrow().users;
		for (id, other) in users.iter() {
			if *id == user.id {
				continue;
			}

			other.send(stream);
		}
	}

	fn disconnect_user(&self, user: &User) {
		let users = &self.user_list.borrow().users;
		for id in user.get_aura() {
			let other = match users.get(&id) {
				Some(u) => u,
				None => continue,
			};

			other.remove_aura(&user.id);

			other.send(&ByteWriter::general_message(
				other.id,
				other.id,
				Opcode::SMsgUserLeft,
				&ByteWriter::new().write_i32(user.id),
			));
		}
	}

	fn broadcast_user_count(&self) {
		self.send_to_all(&ByteWriter::general_message(
			0,
			0,
			Opcode::SMsgUserCount,
			&ByteWriter::new()
				.write_u8(1)
				.write_i32(self.user_list.borrow().users.len() as i32),
		));
	}

	fn new_user(&self, user: &User, name: String, avatar: String) {
		self.lua_api.new_user(user, &name, &avatar);

		match self.user_list.borrow().get_master() {
			Some(master) => {
				if user.id != master.id {
					user.send(&ByteWriter::general_message(
						user.id,
						user.id,
						Opcode::SMsgSetMaster,
						&ByteWriter::new().write_u8(0),
					));
				}
			}
			None => (), // Unreachable?
		};

		self.broadcast_user_count();
	}

	fn position_update(&self, user: &User, pos: Vector3) {
		self.lua_api.pos_update(user, &pos);

		self.update_aura(user);
		self.send_to_aura(user, &ByteWriter::position_update(user.id, &pos));
	}

	fn transform_update(&self, user: &User, mat: Mat3, pos: Vector3) {
		self.lua_api.trans_update(user);

		let mut content = ByteWriter::new();
		for i in 0..9 {
			content = content.write_f32(mat.data[i]);
		}
		content = content.write_f32(pos.x).write_f32(pos.y).write_f32(pos.z);

		self.update_aura(user);
		self.send_to_aura(
			user,
			&ByteWriter::message_common(
				user.id,
				user.id,
				user.id,
				MsgCommon::TransformUpdate,
				1,
				&content,
			),
		);
	}

	fn chat_send(&self, user: &User, mut msg: String) {
		if let Some(msg_override) = self.lua_api.chat_send(user, &msg) {
			if msg_override.len() == 0 {
				user.send_msg("Your message was hidden.");
				return;
			}

			msg = msg_override;
			user.send_msg(format!("Your message was replaced with '{}'", msg).as_str())
		}

		let text = format!("{}: {}", user.get_name(), msg).to_string();

		self.send_to_others(
			user,
			&ByteWriter::message_common(
				user.id,
				user.id,
				user.id,
				MsgCommon::ChatSend,
				1,
				&ByteWriter::new().write_string(&text),
			),
		)
	}

	fn character_update(&self, user: &User, data: String) {
		self.send_to_aura(
			user,
			&ByteWriter::message_common(
				user.id,
				user.id,
				user.id,
				MsgCommon::CharacterUpdate,
				1,
				&ByteWriter::new().write_string(&data),
			),
		)
	}

	fn name_change(&self, user: &User, name: String) {
		self.lua_api.name_change(user, &name);

		self.send_to_others(
			user,
			&ByteWriter::message_common(
				user.id,
				user.id,
				user.id,
				MsgCommon::NameChange,
				1,
				&ByteWriter::new().write_string(&name),
			),
		)
	}

	fn avatar_change(&self, user: &User, avatar: String) {
		self.lua_api.avatar_change(user, &avatar);

		self.send_to_others(
			user,
			&ByteWriter::message_common(
				user.id,
				user.id,
				user.id,
				MsgCommon::AvatarChange,
				1,
				&ByteWriter::new().write_string(&avatar),
			),
		)
	}

	fn private_chat(&self, user: &User, receiver: i32, text: String) {
		let users = &self.user_list.borrow().users;
		let other = match users.get(&receiver) {
			Some(u) => u,
			None => return,
		};

		other.send(&ByteWriter::message_common(
			user.id,
			user.id,
			user.id,
			MsgCommon::PrivateChat,
			2,
			&ByteWriter::new().write_i32(user.id).write_string(&text),
		))
	}

	fn appl_specific(
		&self,
		user: &User,
		strategy: u8,
		id: i32,
		method: String,
		strarg: String,
		intarg: i32,
	) {
		let stream = ByteWriter::message_common(
			user.id,
			user.id,
			id,
			MsgCommon::ApplSpecific,
			strategy,
			&ByteWriter::new()
				.write_u8(2)
				.write_string(&method)
				.write_string(&strarg)
				.write_i32(intarg),
		);

		if id == -9999 {
			match strategy {
				// This could be wrong... :3c
				0 | 3 | 5 => self.send_to_all(&stream),
				1 | 4 | 6 => self.send_to_others(user, &stream),
				2 => match self.user_list.borrow().get_master() {
					Some(master) => master.send(&stream),
					None => return,
				},
				_ => (),
			}

			return;
		}

		match strategy {
			0 => self.send_to_aura_inclusive(user, &stream),
			1 => self.send_to_aura(user, &stream),
			2 => {
				let users = &self.user_list.borrow().users;
				let target = match users.get(&id) {
					Some(u) => u,
					None => return,
				};
				target.send(&stream);
			}
			3 => self.send_to_all(&stream),
			4 => self.send_to_others(user, &stream),
			_ => (),
		}
	}
}
