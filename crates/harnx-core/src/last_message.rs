//! `LastMessage` — snapshot of the most recent user prompt + assistant
//! output, kept on Config for "continue last message" flows.

use crate::input::Input;

#[derive(Debug, Clone)]
pub struct LastMessage {
    pub input: Input,
    pub output: String,
    pub continuous: bool,
}

impl LastMessage {
    pub fn new(input: Input, output: String) -> Self {
        Self {
            input,
            output,
            continuous: true,
        }
    }
}
