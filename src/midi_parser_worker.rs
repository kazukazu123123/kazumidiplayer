use kazumidiparser::MIDIPlayerData;
use std::ffi::c_void;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

pub enum ParserCommand {
    Load(String, kazumidiparser::CancelToken),
}

pub enum ParserFeedbackCommand {
    ParseComplete(Arc<MIDIPlayerData>, String, kazumidiparser::CancelToken),
    ParseCancelled(kazumidiparser::CancelToken),
    ParseFailed(String, kazumidiparser::CancelToken),
    Progress(u16, u16),
}

#[derive(Clone)]
pub struct MIDIParserWorker {
    tx: Sender<ParserCommand>,
}

unsafe extern "C" fn progress_callback(
    current_track: u16,
    total_tracks: u16,
    user_data: *mut c_void,
) {
    if !user_data.is_null() {
        // Safety: user_data is guaranteed non-null by the check above.
        // The pointer is to a Sender<ParserFeedbackCommand> that outlives the callback.
        let tx = unsafe { &*(user_data as *const Sender<ParserFeedbackCommand>) };
        let _ = tx.send(ParserFeedbackCommand::Progress(current_track, total_tracks));
    }
}

impl MIDIParserWorker {
    pub fn spawn(tx_feedback_to_player: Sender<ParserFeedbackCommand>) -> Self {
        let (tx, rx) = mpsc::channel::<ParserCommand>();

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

                        thread::spawn(move || {
                            let tx_for_progress = tx_feedback.clone();
                            let tx_ptr =
                                &tx_for_progress as *const Sender<ParserFeedbackCommand>
                                    as *mut c_void;

                            let parse_result = MIDIPlayerData::parse(
                                &path,
                                Some(progress_callback),
                                tx_ptr,
                                Some(&token_clone),
                            );
                            match parse_result {
                                Ok(data) => {
                                    let _ = tx_feedback.send(
                                        ParserFeedbackCommand::ParseComplete(
                                            Arc::new(data),
                                            path,
                                            token_clone,
                                        ),
                                    );
                                }
                                Err(kazumidiparser::Error::Cancelled) => {
                                    let _ = tx_feedback
                                        .send(ParserFeedbackCommand::ParseCancelled(token_clone));
                                }
                                Err(_) => {
                                    let _ = tx_feedback
                                        .send(ParserFeedbackCommand::ParseFailed(path, token_clone));
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

        Self { tx }
    }

    pub fn send(&self, cmd: ParserCommand) {
        self.tx.send(cmd).ok();
    }
}
