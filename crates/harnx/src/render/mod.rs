mod markdown;

pub use self::markdown::{MarkdownRender, RenderOptions};

use crate::utils::{error_text, pretty_error};

pub fn render_error(err: anyhow::Error) {
    eprintln!("{}", error_text(&pretty_error(&err)));
}
