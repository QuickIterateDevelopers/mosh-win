// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! The interactive client main loop (port of mosh's STMClient).

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::complete::Complete;
use crate::console;
use crate::network::{Connection, Datagram};
use crate::overlay::{DisplayPreference, OverlayManager};
use crate::terminal::display::Display;
use crate::terminal::framebuffer::Framebuffer;
use crate::timestamp::timestamp;
use crate::transport::Transport;
use crate::user::{UserEvent, UserStream};

pub enum Event {
    Net(Datagram),
    Input(Vec<u8>),
    Resize,
    InputEof,
    /// Headless testing: print the current screen to stdout.
    Dump,
}

/// Scripted input for non-interactive testing: (delay before sending in ms, bytes).
pub struct Headless {
    pub size: (i32, i32),
    pub script: Vec<(u64, Vec<u8>)>,
}

pub struct ClientConfig {
    pub addr: SocketAddr,
    pub key: [u8; 16],
    pub predict: DisplayPreference,
    pub predict_overwrite: bool,
    pub headless: Option<Headless>,
}

struct Client {
    display: Display,
    overlays: OverlayManager,
    network: Transport,
    local_framebuffer: Framebuffer,
    window_size: (i32, i32),
    repaint_requested: bool,
    quit_sequence_started: bool,
    lf_entered: bool,
    escape_key: i32,
    escape_pass_key: u8,
    escape_pass_key2: u8,
    escape_requires_lf: bool,
    escape_key_help: String,
    connecting_notification: String,
    clean_shutdown: bool,
    headless: bool,
}

fn escape_key_config() -> (i32, u8, u8) {
    const DEFAULT: (i32, u8, u8) = (0x1E, b'^', b'^');
    let Ok(env) = std::env::var("MOSH_ESCAPE_KEY") else { return DEFAULT };
    let bytes = env.as_bytes();
    let cfg = match bytes.len() {
        0 => (-1, 0, 0),
        1 if bytes[0] > 0 && bytes[0] < 128 => {
            let key = bytes[0];
            let pass = if key < 32 { key + b'@' } else { key };
            let pass2 = if pass.is_ascii_uppercase() { pass.to_ascii_lowercase() } else { pass };
            (key as i32, pass, pass2)
        }
        _ => DEFAULT,
    };
    // Don't allow Ctrl-C, Ctrl-D, NewLine, Ctrl-L or CarriageReturn as the escape key.
    if matches!(cfg.0, 0x03 | 0x04 | 0x0A | 0x0C | 0x0D) { DEFAULT } else { cfg }
}

impl Client {
    fn write(&self, s: &str) {
        if !self.headless {
            console::write_out(s);
        }
    }

    fn still_connecting(&self) -> bool {
        self.network.latest_remote_state().2 == 0
    }

    fn output_new_frame(&mut self) {
        /* fetch target state */
        let mut new_state = self.network.latest_remote_state().0.fb().clone();
        /* apply local overlays */
        self.overlays.apply(&mut new_state);
        /* calculate minimal difference from where we are */
        let diff = self.display.new_frame(!self.repaint_requested, &self.local_framebuffer, &new_state);
        self.write(&diff);
        self.repaint_requested = false;
        self.local_framebuffer = new_state;
    }

    fn process_network_input(&mut self, d: Datagram) {
        if let Err(e) = self.network.recv(&d.0) {
            self.overlays.notifications.set_network_error(&e);
        }
        let (state, ts, _) = self.network.latest_remote_state();
        let echo_ack = state.echo_ack();
        /* Now give hints to the overlays */
        self.overlays.notifications.server_heard(ts);
        self.overlays.notifications.server_acked(self.network.sent_state_acked_timestamp());
        let p = &mut self.overlays.predictions;
        p.set_local_frame_acked(self.network.sent_state_acked());
        p.set_send_interval(self.network.send_interval());
        p.set_local_frame_late_acked(echo_ack);
    }

    /// Returns false if the user wants to quit.
    fn process_user_input(&mut self, buf: &[u8]) -> bool {
        if self.network.shutdown_in_progress() {
            return true;
        }
        let sent_last = self.network.sent_state_last();
        self.overlays.predictions.set_local_frame_sent(sent_last);

        /* Don't predict for bulk data. */
        let paste = buf.len() > 100;
        if paste {
            self.overlays.predictions.reset();
        }

        for &the_byte in buf {
            if !paste {
                self.overlays.predictions.new_user_byte(the_byte, &self.local_framebuffer);
            }

            if self.quit_sequence_started {
                if the_byte == b'.' {
                    /* Quit sequence is Ctrl-^ . */
                    if !self.network.shutdown_in_progress() {
                        self.overlays.notifications.set_notification_string("Exiting on user request...", true, true);
                        self.network.start_shutdown();
                        return true;
                    }
                    return false;
                } else if the_byte == self.escape_pass_key || the_byte == self.escape_pass_key2 {
                    /* Emulation sequence to type escape_key is escape_key + escape_pass_key */
                    self.network.current_state_mut().push_back(UserEvent::Byte(self.escape_key as u8));
                } else {
                    /* Escape key followed by anything other than . and ^ gets sent literally */
                    let cs = self.network.current_state_mut();
                    cs.push_back(UserEvent::Byte(self.escape_key as u8));
                    cs.push_back(UserEvent::Byte(the_byte));
                }
                self.quit_sequence_started = false;
                if self.overlays.notifications.notification_string() == self.escape_key_help {
                    self.overlays.notifications.set_notification_string("", false, true);
                }
                continue;
            }

            self.quit_sequence_started = self.escape_key > 0
                && the_byte as i32 == self.escape_key
                && (self.lf_entered || !self.escape_requires_lf);
            if self.quit_sequence_started {
                self.lf_entered = false;
                let help = self.escape_key_help.clone();
                self.overlays.notifications.set_notification_string(&help, true, false);
                continue;
            }

            self.lf_entered = the_byte == 0x0A || the_byte == 0x0D;
            if the_byte == 0x0C {
                /* Ctrl-L */
                self.repaint_requested = true;
            }
            self.network.current_state_mut().push_back(UserEvent::Byte(the_byte));
        }
        true
    }

    fn process_resize(&mut self, size: (i32, i32)) {
        self.window_size = size;
        if !self.network.shutdown_in_progress() {
            self.network.current_state_mut().push_back(UserEvent::Resize(size.0, size.1));
        }
        /* tell prediction engine */
        self.overlays.predictions.reset();
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Net(d) => self.process_network_input(d),
            Event::Input(bytes) => {
                if !self.process_user_input(&bytes) {
                    self.request_exit("Exiting...");
                }
            }
            Event::InputEof => self.request_exit("Exiting..."),
            Event::Resize => {
                if !self.headless {
                    if let Some(sz) = console::window_size() {
                        if sz != self.window_size {
                            self.process_resize(sz);
                        }
                    }
                }
            }
            Event::Dump => {
                print!("{}", self.network.latest_remote_state().0.fb().to_text());
            }
        }
    }

    fn request_exit(&mut self, msg: &str) {
        if !self.network.shutdown_in_progress() {
            self.overlays.notifications.set_notification_string(msg, true, true);
            self.network.start_shutdown();
        }
    }

    fn main_loop(&mut self, rx: &Receiver<Event>) {
        loop {
            self.output_new_frame();

            let mut wait_time = self.network.wait_time().min(self.overlays.wait_time());
            /* Handle startup "Connecting..." message */
            if self.still_connecting() {
                wait_time = wait_time.min(250);
            }
            // also poll the window size now and then, in case a resize event was missed
            if !self.headless {
                wait_time = wait_time.min(500);
            }

            match rx.recv_timeout(Duration::from_millis(wait_time)) {
                Ok(ev) => {
                    self.handle_event(ev);
                    while let Ok(ev) = rx.try_recv() {
                        self.handle_event(ev);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if !self.headless {
                if let Some(sz) = console::window_size() {
                    if sz != self.window_size {
                        self.process_resize(sz);
                    }
                }
            }

            /* quit if our shutdown has been acknowledged */
            if self.network.shutdown_in_progress() && self.network.shutdown_acknowledged() {
                self.clean_shutdown = true;
                break;
            }
            /* quit after shutdown acknowledgement timeout */
            if self.network.shutdown_in_progress() && self.network.shutdown_ack_timed_out() {
                break;
            }
            /* quit if we received and acknowledged a shutdown request */
            if self.network.counterparty_shutdown_ack_sent() {
                self.clean_shutdown = true;
                break;
            }

            /* write diagnostic message if can't reach server */
            let latest_ts = self.network.latest_remote_state().1;
            if self.still_connecting()
                && !self.network.shutdown_in_progress()
                && timestamp().wrapping_sub(latest_ts) > 250
            {
                if timestamp().wrapping_sub(latest_ts) > 15000 {
                    self.request_exit("Timed out waiting for server...");
                } else {
                    let msg = self.connecting_notification.clone();
                    self.overlays.notifications.set_notification_string(&msg, false, true);
                }
            } else if !self.still_connecting()
                && self.overlays.notifications.notification_string() == self.connecting_notification
            {
                self.overlays.notifications.set_notification_string("", false, true);
            }

            self.network.tick();

            if let Some(err) = self.network.connection.take_send_error() {
                self.overlays.notifications.set_network_error(&err);
            } else {
                self.overlays.notifications.clear_network_error();
            }
        }
    }

    fn shutdown(&mut self) {
        /* Restore screen state */
        self.overlays.notifications.set_notification_string("", false, true);
        self.overlays.notifications.server_heard(timestamp());
        self.overlays.set_title_prefix("");
        self.output_new_frame();
        self.write(&self.display.close());
    }
}

/// Runs a session. Returns Ok(true) on a clean shutdown.
pub fn run(cfg: ClientConfig) -> anyhow::Result<bool> {
    let (tx, rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    let headless = cfg.headless.is_some();

    let window_size = match &cfg.headless {
        Some(h) => h.size,
        None => console::window_size().unwrap_or((80, 24)),
    };

    let raw = if headless { None } else { Some(console::RawConsole::enter()?) };

    let connection = Connection::new(&cfg.key, cfg.addr, tx.clone())?;
    let port = cfg.addr.port();
    let mut network = Transport::new(connection, UserStream::default(), Complete::new(window_size.0, window_size.1));
    network.set_send_delay(1); /* minimal delay on outgoing keystrokes */
    /* tell server the size of the terminal */
    network.current_state_mut().push_back(UserEvent::Resize(window_size.0, window_size.1));

    let (escape_key, escape_pass_key, escape_pass_key2) = escape_key_config();
    let mut overlays = OverlayManager::new();
    overlays.predictions.set_display_preference(cfg.predict);
    overlays.predictions.set_predict_overwrite(cfg.predict_overwrite);
    if std::env::var_os("MOSH_TITLE_NOPREFIX").is_none() {
        overlays.set_title_prefix("[mosh] ");
    }

    let mut escape_requires_lf = false;
    let mut escape_key_help = String::new();
    if escape_key > 0 {
        let pass_name = format!("\"{}\"", escape_pass_key as char);
        let key_name = if escape_key < 32 {
            format!("Ctrl-{}", escape_pass_key as char)
        } else {
            escape_requires_lf = true;
            format!("\"{}\"", escape_key as u8 as char)
        };
        escape_key_help = format!("Commands: \".\" quits, {pass_name} gives literal {key_name}");
        overlays.notifications.set_escape_key_string(&key_name);
    }

    let display = Display::new();
    let mut client = Client {
        local_framebuffer: Framebuffer::new(window_size.0, window_size.1),
        display,
        overlays,
        network,
        window_size,
        repaint_requested: false,
        quit_sequence_started: false,
        lf_entered: false,
        escape_key,
        escape_pass_key,
        escape_pass_key2,
        escape_requires_lf,
        escape_key_help,
        connecting_notification: format!("Nothing received from server on UDP port {port}."),
        clean_shutdown: false,
        headless,
    };

    /* Put terminal in application-cursor-key mode, and initialize screen */
    client.write(&client.display.open());
    let init = client.display.new_frame(false, &client.local_framebuffer, &client.local_framebuffer);
    client.write(&init);

    match cfg.headless {
        Some(h) => {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for (delay, bytes) in h.script {
                    std::thread::sleep(Duration::from_millis(delay));
                    let ev = if bytes.is_empty() { Event::Dump } else { Event::Input(bytes) };
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Event::InputEof);
            });
        }
        None => console::spawn_input_thread(tx.clone()),
    }
    drop(tx);

    client.main_loop(&rx);
    client.shutdown();
    drop(raw);

    if client.still_connecting() {
        eprintln!(
            "\nmosh did not make a successful connection to {}.\n\
             Please verify that UDP port {} is not firewalled and can reach the server.\n\n\
             (By default, mosh uses a UDP port between 60000 and 61000. The -p option\n\
             selects a specific UDP port number.)",
            cfg.addr, port
        );
    } else if !client.clean_shutdown {
        eprintln!(
            "\n\nmosh did not shut down cleanly. Please note that the\n\
             mosh-server process may still be running on the server."
        );
    }
    Ok(client.clean_shutdown)
}
