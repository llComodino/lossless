use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::thread;

use crossterm::event::{self, Event};

use crate::conn::Msg;

/// Lets the input thread read one more event. The main loop opens it after
/// handling each event, so while it runs a script on the terminal nobody
/// else reads the keyboard.
pub struct Gate(SyncSender<()>);

impl Gate {
    pub fn open(&self) {
        let _ = self.0.try_send(());
    }
}

pub fn spawn(tx: Sender<Msg>) -> Gate {
    let (gate, permits) = sync_channel(1);
    let _ = gate.send(());
    thread::spawn(move || read(&tx, &permits));
    Gate(gate)
}

fn read(tx: &Sender<Msg>, permits: &Receiver<()>) {
    while permits.recv().is_ok() {
        let msg = loop {
            match event::read() {
                Ok(Event::Key(k)) => break Msg::Key(k),
                Ok(Event::Resize(..)) => break Msg::Resize,
                Ok(_) => {}
                Err(_) => return,
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}
