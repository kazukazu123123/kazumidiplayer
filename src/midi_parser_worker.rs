use kazumidiparser::MIDIPlayerData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// A parse-progress snapshot: tracks done and a live per-event count, which is
/// approximate while tracks are in flight and exact once parsing ends.
pub use kazumidiparser::Progress as ParseProgress;

pub enum ParserCommand {
    Load(String, kazumidiparser::CancelToken),
}

pub enum ParserFeedbackCommand {
    ParseComplete(Arc<MIDIPlayerData>, String, kazumidiparser::CancelToken, Duration),
    ParseCancelled(kazumidiparser::CancelToken),
    ParseFailed(String, kazumidiparser::CancelToken),
    Progress(ParseProgress),
}

#[derive(Clone)]
pub struct MIDIParserWorker {
    tx: Sender<ParserCommand>,
    /// Thread count handed to `MIDIPlayerData::parse`. Read when a parse starts, so
    /// a change applies to the next load, not the one already running.
    threads: Arc<AtomicU32>,
}

impl MIDIParserWorker {
    pub fn spawn(tx_feedback_to_player: Sender<ParserFeedbackCommand>) -> Self {
        let (tx, rx) = mpsc::channel::<ParserCommand>();
        let threads = Arc::new(AtomicU32::new(crate::app_state::cpu_threads()));
        let threads_for_thread = threads.clone();

        thread::spawn(move || {
            let mut current_token: Option<kazumidiparser::CancelToken> = None;

            loop {
                match rx.recv() {
                    Ok(ParserCommand::Load(path, token)) => {
                        // Cancel previous parse
                        if let Some(prev_token) = current_token.take() {
                            prev_token.cancel();
                        }

                        let tx_feedback = tx_feedback_to_player.clone();
                        let token_clone = token.clone();
                        current_token = Some(token);
                        let threads = threads_for_thread.load(Ordering::Relaxed).max(1);

                        thread::spawn(move || {
                            let tx_for_progress = tx_feedback.clone();
                            let mut on_progress = |p: ParseProgress| {
                                let _ = tx_for_progress.send(ParserFeedbackCommand::Progress(p));
                            };

                            let parse_started = Instant::now();
                            let parse_result = MIDIPlayerData::parse(
                                &path,
                                threads,
                                Some(&mut on_progress),
                                Some(&token_clone),
                            );
                            let parse_duration = parse_started.elapsed();
                            match parse_result {
                                Ok(data) => {
                                    let _ = tx_feedback.send(ParserFeedbackCommand::ParseComplete(
                                        Arc::new(data),
                                        path,
                                        token_clone,
                                        parse_duration,
                                    ));
                                }
                                Err(kazumidiparser::Error::Cancelled) => {
                                    let _ = tx_feedback
                                        .send(ParserFeedbackCommand::ParseCancelled(token_clone));
                                }
                                Err(_) => {
                                    let _ = tx_feedback.send(ParserFeedbackCommand::ParseFailed(
                                        path,
                                        token_clone,
                                    ));
                                }
                            }
                        });
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        Self { tx, threads }
    }

    pub fn send(&self, cmd: ParserCommand) {
        self.tx.send(cmd).ok();
    }

    pub fn set_threads(&self, threads: u32) {
        self.threads.store(threads.max(1), Ordering::Relaxed);
    }
}
