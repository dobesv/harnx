//! Per-subscriber AG-UI lifecycle validation and repair.

use std::{collections::HashSet, hash::Hash};

use ag_ui_core::{
    event::{
        BaseEvent, Event, MessagesSnapshotEvent, StepFinishedEvent, StepStartedEvent,
        TextMessageContentEvent, TextMessageEndEvent, TextMessageStartEvent, ThinkingEndEvent,
        ThinkingStartEvent, ThinkingTextMessageContentEvent, ThinkingTextMessageEndEvent,
        ThinkingTextMessageStartEvent, ToolCallArgsEvent, ToolCallEndEvent, ToolCallResultEvent,
        ToolCallStartEvent,
    },
    types::{
        ids::{MessageId, ToolCallId},
        message::Role,
    },
};
use bytes::Bytes;

use crate::ag_ui::frame_event;

#[derive(Default)]
pub(crate) struct LiveStreamGuard {
    started_text_messages: HashSet<MessageId>,
    seen_tool_call_ids: Vec<ToolCallId>,
    started_tool_call_ids: Vec<ToolCallId>,
    started_steps: HashSet<String>,
    thinking_open: bool,
    thinking_text_open: bool,
}

impl LiveStreamGuard {
    /// Close every lifecycle opened on this subscriber's stream.
    ///
    /// A snapshot hydrates content but does not update the AG-UI client's lifecycle
    /// verifier, so only starts forwarded by this guard are eligible for these closes.
    pub(crate) fn finalize_open_lifecycles(&mut self) -> Bytes {
        let mut frames = String::new();

        for message_id in self.started_text_messages.drain() {
            push_frame(
                &mut frames,
                Event::TextMessageEnd(TextMessageEndEvent {
                    base: synthetic_base(),
                    message_id,
                }),
            )
            .expect("synthetic text end should serialize");
        }
        for step_name in self.started_steps.drain() {
            push_frame(
                &mut frames,
                Event::StepFinished(StepFinishedEvent {
                    base: synthetic_base(),
                    step_name,
                }),
            )
            .expect("synthetic step finish should serialize");
        }
        for tool_call_id in self.started_tool_call_ids.drain(..) {
            push_frame(
                &mut frames,
                Event::ToolCallEnd(ToolCallEndEvent {
                    base: synthetic_base(),
                    tool_call_id,
                }),
            )
            .expect("synthetic tool call end should serialize");
        }
        if self.thinking_text_open {
            push_frame(
                &mut frames,
                Event::ThinkingTextMessageEnd(ThinkingTextMessageEndEvent {
                    base: synthetic_base(),
                }),
            )
            .expect("synthetic thinking text end should serialize");
            self.thinking_text_open = false;
        }
        if self.thinking_open {
            push_frame(
                &mut frames,
                Event::ThinkingEnd(ThinkingEndEvent {
                    base: synthetic_base(),
                }),
            )
            .expect("synthetic thinking end should serialize");
            self.thinking_open = false;
        }

        Bytes::from(frames)
    }

    fn reset_after_snapshot(&mut self) -> Bytes {
        let frames = self.finalize_open_lifecycles();
        self.seen_tool_call_ids.clear();
        frames
    }

    fn frame_text_event(&mut self, event: Event) -> Option<Bytes> {
        match event {
            Event::TextMessageStart(event) => self.frame_text_start(event),
            Event::TextMessageContent(event) => self.frame_text_content(event),
            Event::TextMessageEnd(event) => self.frame_text_end(event),
            _ => unreachable!("text dispatch received non-text event"),
        }
    }

    fn frame_text_start(&mut self, event: TextMessageStartEvent) -> Option<Bytes> {
        self.started_text_messages.insert(event.message_id.clone());
        frame(Event::TextMessageStart(event))
    }

    fn frame_text_content(&mut self, event: TextMessageContentEvent) -> Option<Bytes> {
        let message_id = event.message_id.clone();
        frame_with_start(
            &mut self.started_text_messages,
            message_id.clone(),
            Event::TextMessageStart(TextMessageStartEvent {
                base: synthetic_base(),
                message_id,
                role: Role::Assistant,
            }),
            Event::TextMessageContent(event),
        )
    }

    fn frame_text_end(&mut self, event: TextMessageEndEvent) -> Option<Bytes> {
        let message_id = event.message_id.clone();
        frame_end_with_start(
            &mut self.started_text_messages,
            message_id.clone(),
            Event::TextMessageStart(TextMessageStartEvent {
                base: synthetic_base(),
                message_id,
                role: Role::Assistant,
            }),
            Event::TextMessageEnd(event),
        )
    }

    fn frame_tool_event(&mut self, event: Event) -> Option<Bytes> {
        match event {
            Event::ToolCallStart(event) => self.frame_tool_start(event),
            Event::ToolCallArgs(event) => self.frame_tool_args(event),
            Event::ToolCallEnd(event) => self.frame_tool_end(event),
            Event::ToolCallResult(event) => self.frame_tool_result(event),
            _ => unreachable!("tool dispatch received non-tool event"),
        }
    }

    fn frame_tool_start(&mut self, event: ToolCallStartEvent) -> Option<Bytes> {
        if !self
            .seen_tool_call_ids
            .iter()
            .any(|seen| seen == &event.tool_call_id)
        {
            self.seen_tool_call_ids.push(event.tool_call_id.clone());
        }
        if !self
            .started_tool_call_ids
            .iter()
            .any(|started| started == &event.tool_call_id)
        {
            self.started_tool_call_ids.push(event.tool_call_id.clone());
        }
        frame(Event::ToolCallStart(event))
    }

    fn frame_tool_args(&self, event: ToolCallArgsEvent) -> Option<Bytes> {
        self.started_tool_call_ids
            .iter()
            .any(|started| started == &event.tool_call_id)
            .then(|| frame(Event::ToolCallArgs(event)))
            .flatten()
    }

    fn frame_tool_end(&mut self, event: ToolCallEndEvent) -> Option<Bytes> {
        let position = self
            .started_tool_call_ids
            .iter()
            .position(|started| started == &event.tool_call_id)?;
        self.started_tool_call_ids.remove(position);
        frame(Event::ToolCallEnd(event))
    }

    fn frame_tool_result(&self, event: ToolCallResultEvent) -> Option<Bytes> {
        self.seen_tool_call_ids
            .iter()
            .any(|seen| seen == &event.tool_call_id)
            .then(|| frame(Event::ToolCallResult(event)))
            .flatten()
    }

    fn frame_step_event(&mut self, event: Event) -> Option<Bytes> {
        match event {
            Event::StepStarted(event) => self.frame_step_started(event),
            Event::StepFinished(event) => self.frame_step_finished(event),
            _ => unreachable!("step dispatch received non-step event"),
        }
    }

    fn frame_step_started(&mut self, event: StepStartedEvent) -> Option<Bytes> {
        self.started_steps.insert(event.step_name.clone());
        frame(Event::StepStarted(event))
    }

    fn frame_step_finished(&mut self, event: StepFinishedEvent) -> Option<Bytes> {
        let step_name = event.step_name.clone();
        frame_end_with_start(
            &mut self.started_steps,
            step_name.clone(),
            Event::StepStarted(StepStartedEvent {
                base: synthetic_base(),
                step_name,
            }),
            Event::StepFinished(event),
        )
    }

    fn frame_thinking_event(&mut self, event: Event) -> Option<Bytes> {
        match event {
            Event::ThinkingStart(event) => self.frame_thinking_start(event),
            Event::ThinkingEnd(event) => self.frame_thinking_end(event),
            Event::ThinkingTextMessageStart(event) => self.frame_thinking_text_start(event),
            Event::ThinkingTextMessageContent(event) => self.frame_thinking_text_content(event),
            Event::ThinkingTextMessageEnd(event) => self.frame_thinking_text_end(event),
            _ => unreachable!("thinking dispatch received non-thinking event"),
        }
    }

    fn frame_thinking_start(&mut self, event: ThinkingStartEvent) -> Option<Bytes> {
        self.thinking_open = true;
        frame(Event::ThinkingStart(event))
    }

    fn frame_thinking_end(&mut self, event: ThinkingEndEvent) -> Option<Bytes> {
        let mut frames = String::new();
        if !self.thinking_open {
            push_frame(&mut frames, synthetic_thinking_start())?;
        }
        if self.thinking_text_open {
            push_frame(&mut frames, synthetic_thinking_text_end())?;
            self.thinking_text_open = false;
        }
        let frame = finish_frames(frames, Event::ThinkingEnd(event))?;
        self.thinking_open = false;
        Some(frame)
    }

    fn frame_thinking_text_start(&mut self, event: ThinkingTextMessageStartEvent) -> Option<Bytes> {
        let mut frames = String::new();
        ensure_open(
            &mut self.thinking_open,
            &mut frames,
            synthetic_thinking_start(),
        )?;
        self.thinking_text_open = true;
        finish_frames(frames, Event::ThinkingTextMessageStart(event))
    }

    fn frame_thinking_text_content(
        &mut self,
        event: ThinkingTextMessageContentEvent,
    ) -> Option<Bytes> {
        let mut frames = String::new();
        ensure_open(
            &mut self.thinking_open,
            &mut frames,
            synthetic_thinking_start(),
        )?;
        ensure_open(
            &mut self.thinking_text_open,
            &mut frames,
            synthetic_thinking_text_start(),
        )?;
        finish_frames(frames, Event::ThinkingTextMessageContent(event))
    }

    fn frame_thinking_text_end(&mut self, event: ThinkingTextMessageEndEvent) -> Option<Bytes> {
        let mut frames = String::new();
        ensure_open(
            &mut self.thinking_open,
            &mut frames,
            synthetic_thinking_start(),
        )?;
        if !self.thinking_text_open {
            push_frame(&mut frames, synthetic_thinking_text_start())?;
        }
        let frame = finish_frames(frames, Event::ThinkingTextMessageEnd(event))?;
        self.thinking_text_open = false;
        Some(frame)
    }

    fn frame_messages_snapshot(&mut self, event: MessagesSnapshotEvent) -> Option<Bytes> {
        let mut frames = self.reset_after_snapshot().to_vec();
        frames.extend_from_slice(
            frame_event(&Event::MessagesSnapshot(event))
                .ok()?
                .as_bytes(),
        );
        Some(Bytes::from(frames))
    }
}

pub(crate) fn frame_guarded_live_event(event: Event, guard: &mut LiveStreamGuard) -> Option<Bytes> {
    match event {
        event @ (Event::TextMessageStart(_)
        | Event::TextMessageContent(_)
        | Event::TextMessageEnd(_)) => guard.frame_text_event(event),
        event @ (Event::ToolCallStart(_)
        | Event::ToolCallArgs(_)
        | Event::ToolCallEnd(_)
        | Event::ToolCallResult(_)) => guard.frame_tool_event(event),
        event @ (Event::StepStarted(_) | Event::StepFinished(_)) => guard.frame_step_event(event),
        event @ (Event::ThinkingStart(_)
        | Event::ThinkingEnd(_)
        | Event::ThinkingTextMessageStart(_)
        | Event::ThinkingTextMessageContent(_)
        | Event::ThinkingTextMessageEnd(_)) => guard.frame_thinking_event(event),
        event @ (Event::RunFinished(_) | Event::RunError(_)) => frame(event),
        Event::MessagesSnapshot(event) => guard.frame_messages_snapshot(event),
        other => frame(other),
    }
}

fn frame(event: Event) -> Option<Bytes> {
    frame_event(&event).ok().map(Bytes::from)
}

fn push_frame(frames: &mut String, event: Event) -> Option<()> {
    frames.push_str(&frame_event(&event).ok()?);
    Some(())
}

fn frame_with_start<T: Eq + Hash>(
    started: &mut HashSet<T>,
    id: T,
    start_event: Event,
    event: Event,
) -> Option<Bytes> {
    let mut frames = String::new();
    if started.insert(id) {
        push_frame(&mut frames, start_event)?;
    }
    finish_frames(frames, event)
}

fn frame_end_with_start<T: Eq + Hash + Clone>(
    started: &mut HashSet<T>,
    id: T,
    start_event: Event,
    end_event: Event,
) -> Option<Bytes> {
    let frame = frame_with_start(started, id.clone(), start_event, end_event)?;
    started.remove(&id);
    Some(frame)
}

fn ensure_open(open: &mut bool, frames: &mut String, start_event: Event) -> Option<()> {
    if !*open {
        push_frame(frames, start_event)?;
        *open = true;
    }
    Some(())
}

fn finish_frames(mut frames: String, event: Event) -> Option<Bytes> {
    push_frame(&mut frames, event)?;
    Some(Bytes::from(frames))
}

fn synthetic_thinking_start() -> Event {
    Event::ThinkingStart(ThinkingStartEvent {
        base: synthetic_base(),
        title: None,
    })
}

fn synthetic_thinking_text_start() -> Event {
    Event::ThinkingTextMessageStart(ThinkingTextMessageStartEvent {
        base: synthetic_base(),
    })
}

fn synthetic_thinking_text_end() -> Event {
    Event::ThinkingTextMessageEnd(ThinkingTextMessageEndEvent {
        base: synthetic_base(),
    })
}

fn synthetic_base() -> BaseEvent {
    BaseEvent {
        timestamp: None,
        raw_event: None,
    }
}
