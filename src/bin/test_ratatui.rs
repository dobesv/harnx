use ratatui::symbols;
use ratatui::widgets::{Block, Borders};
fn main() {
    let _b = Block::default()
        .borders(Borders::TOP)
        .border_set(symbols::border::EMPTY);
}
