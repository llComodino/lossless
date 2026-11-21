use std::sync::mpsc::Sender;
use std::thread;

use crossterm::event::{self, Event};

use crate::conn::Msg;

pub fn spawn(tx: Sender<Msg>) {
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            let msg = match ev {
                Event::Key(k) => Msg::Key(k),
                Event::Resize(..) => Msg::Resize,
                _ => continue,
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
}
