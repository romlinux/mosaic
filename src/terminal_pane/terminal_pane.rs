use ::std::os::unix::io::RawFd;
use ::nix::pty::Winsize;
use ::vte::Perform;

use crate::VteEvent;
use crate::boundaries::Rect;
use crate::terminal_pane::Scroll;
use crate::terminal_pane::terminal_character::{
    TerminalCharacter,
    CharacterStyles,
    AnsiCode,
    NamedColor
};

pub struct TerminalPane {
    pub pid: RawFd,
    pub scroll: Scroll,
    pub display_rows: u16,
    pub display_cols: u16,
    pub should_render: bool,
    pub x_coords: u16,
    pub y_coords: u16,
    pending_styles: CharacterStyles,
}

impl Rect for &mut TerminalPane {
    fn x(&self) -> usize {
        self.x_coords as usize
    }
    fn y(&self) -> usize {
        self.y_coords as usize
    }
    fn rows(&self) -> usize {
        self.display_rows as usize
    }
    fn columns(&self) -> usize {
        self.display_cols as usize
    }
}

impl TerminalPane {
    pub fn new (pid: RawFd, ws: Winsize, x_coords: u16, y_coords: u16) -> TerminalPane {
        let scroll = Scroll::new(ws.ws_col as usize, ws.ws_row as usize);
        let pending_styles = CharacterStyles::new();
        TerminalPane {
            pid,
            scroll,
            display_rows: ws.ws_row,
            display_cols: ws.ws_col,
            should_render: true,
            pending_styles,
            x_coords,
            y_coords,
        }
    }
    pub fn handle_event(&mut self, event: VteEvent) {
        match event {
            VteEvent::Print(c) => {
                self.print(c);
                self.should_render = true;
            },
            VteEvent::Execute(byte) => {
                self.execute(byte);
            },
            VteEvent::Hook(params, intermediates, ignore, c) => {
                self.hook(&params, &intermediates, ignore, c);
            },
            VteEvent::Put(byte) => {
                self.put(byte);
            },
            VteEvent::Unhook => {
                self.unhook();
            },
            VteEvent::OscDispatch(params, bell_terminated) => {
                let params: Vec<&[u8]> = params.iter().map(|p| &p[..]).collect();
                self.osc_dispatch(&params[..], bell_terminated);
            },
            VteEvent::CsiDispatch(params, intermediates, ignore, c) => {
                self.csi_dispatch(&params, &intermediates, ignore, c);
            },
            VteEvent::EscDispatch(intermediates, ignore, byte) => {
                self.esc_dispatch(&intermediates, ignore, byte);
            }
        }
    }
    pub fn reduce_width_right(&mut self, count: u16) {
        self.x_coords += count;
        self.display_cols -= count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn reduce_width_left(&mut self, count: u16) {
        self.display_cols -= count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn increase_width_left(&mut self, count: u16) {
        self.x_coords -= count;
        self.display_cols += count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn increase_width_right(&mut self, count: u16) {
        self.display_cols += count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn reduce_height_down(&mut self, count: u16) {
        self.y_coords += count;
        self.display_rows -= count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn increase_height_down(&mut self, count: u16) {
        self.display_rows += count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn increase_height_up(&mut self, count: u16) {
        self.y_coords -= count;
        self.display_rows += count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn reduce_height_up(&mut self, count: u16) {
        self.display_rows -= count;
        self.reflow_lines();
        self.should_render = true;
    }
    pub fn change_size(&mut self, ws: &Winsize) {
        self.display_cols = ws.ws_col;
        self.display_rows = ws.ws_row;
        self.reflow_lines();
        self.should_render = true;
    }
    fn reflow_lines (&mut self) {
        self.scroll.change_size(self.display_cols as usize, self.display_rows as usize);
    }
    pub fn buffer_as_vte_output(&mut self) -> Option<String> {
        if self.should_render {
            let mut vte_output = String::new();
            let buffer_lines = &self.read_buffer_as_lines();
            let display_cols = &self.display_cols;
            let mut character_styles = CharacterStyles::new();
            for (row, line) in buffer_lines.iter().enumerate() {
                vte_output = format!("{}\u{1b}[{};{}H\u{1b}[m", vte_output, self.y_coords as usize + row + 1, self.x_coords + 1); // goto row/col and reset styles
                for (col, t_character) in line.iter().enumerate() {
                    if (col as u16) < *display_cols {
                        // in some cases (eg. while resizing) some characters will spill over
                        // before they are corrected by the shell (for the prompt) or by reflowing
                        // lines
                        if let Some(new_styles) = character_styles.update_and_return_diff(&t_character.styles) {
                            // the terminal keeps the previous styles as long as we're in the same
                            // line, so we only want to update the new styles here (this also
                            // includes resetting previous styles as needed)
                            vte_output = format!("{}{}", vte_output, new_styles);
                        }
                        vte_output.push(t_character.character);
                    }
                }
                character_styles.clear();
            }
            self.should_render = false;
            Some(vte_output)
        } else {
            None
        }
    }
    pub fn read_buffer_as_lines (&self) -> Vec<Vec<TerminalCharacter>> {
        self.scroll.as_character_lines()
    }
    pub fn cursor_coordinates (&self) -> (usize, usize) { // (x, y)
        self.scroll.cursor_coordinates_on_screen()
    }
    pub fn scroll_up(&mut self, count: usize) {
        self.scroll.move_viewport_up(count);
        self.should_render = true;
    }
    pub fn scroll_down(&mut self, count: usize) {
        self.scroll.move_viewport_down(count);
        self.should_render = true;
    }
    pub fn clear_scroll(&mut self) {
        self.scroll.reset_viewport();
        self.should_render = true;
    }
    fn add_newline (&mut self) {
        self.scroll.add_canonical_line(); // TODO: handle scroll region
        self.reset_all_ansi_codes();
        self.should_render = true;
    }
    fn move_to_beginning_of_line (&mut self) {
        self.scroll.move_cursor_to_beginning_of_canonical_line();
    }
    fn move_cursor_backwards(&mut self, count: usize) {
        self.scroll.move_cursor_backwards(count);
    }
    fn reset_all_ansi_codes(&mut self) {
        self.pending_styles.clear();
    }
}

fn debug_log_to_file (message: String, pid: RawFd) {
    if pid == 0 {
        use std::fs::OpenOptions;
        use std::io::prelude::*;
        let mut file = OpenOptions::new().append(true).create(true).open("/tmp/mosaic-log.txt").unwrap();
        file.write_all(message.as_bytes()).unwrap();
        file.write_all("\n".as_bytes()).unwrap();
    }
}

impl vte::Perform for TerminalPane {
    fn print(&mut self, c: char) {
        // apparently, building TerminalCharacter like this without a "new" method
        // is a little faster
        let terminal_character = TerminalCharacter {
            character: c,
            styles: self.pending_styles,
        };
        self.scroll.add_character(terminal_character);
    }

    fn execute(&mut self, byte: u8) {
        if byte == 13 { // 0d, carriage return
            self.move_to_beginning_of_line();
        } else if byte == 08 { // backspace
            self.move_cursor_backwards(1);
        } else if byte == 10 { // 0a, newline
            self.add_newline();
        }
    }

    fn hook(&mut self, _params: &[i64], _intermediates: &[u8], _ignore: bool, _c: char) {
        // TBD
    }

    fn put(&mut self, _byte: u8) {
        // TBD
    }

    fn unhook(&mut self) {
        // TBD
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // TBD
    }

    fn csi_dispatch(&mut self, params: &[i64], _intermediates: &[u8], _ignore: bool, c: char) {
        if c == 'm' {
            if params.is_empty() || params[0] == 0 {
                // reset all
                self.pending_styles.reset_all();
            } else if params[0] == 39 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::Reset));
            } else if params[0] == 49 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::Reset));
            } else if params[0] == 21 {
                // reset bold
                self.pending_styles = self.pending_styles.bold(Some(AnsiCode::Reset));
            } else if params[0] == 22 {
                // reset bold and dim
                self.pending_styles = self.pending_styles.bold(Some(AnsiCode::Reset));
                self.pending_styles = self.pending_styles.dim(Some(AnsiCode::Reset));
            } else if params[0] == 23 {
                // reset italic
                self.pending_styles = self.pending_styles.italic(Some(AnsiCode::Reset));
            } else if params[0] == 24 {
                // reset underline
                self.pending_styles = self.pending_styles.underline(Some(AnsiCode::Reset));
            } else if params[0] == 25 {
                // reset blink
                self.pending_styles = self.pending_styles.blink_slow(Some(AnsiCode::Reset));
                self.pending_styles = self.pending_styles.blink_fast(Some(AnsiCode::Reset));
            } else if params[0] == 27 {
                // reset reverse
                self.pending_styles = self.pending_styles.reverse(Some(AnsiCode::Reset));
            } else if params[0] == 28 {
                // reset hidden
                self.pending_styles = self.pending_styles.hidden(Some(AnsiCode::Reset));
            } else if params[0] == 29 {
                // reset strike
                self.pending_styles = self.pending_styles.strike(Some(AnsiCode::Reset));
            } else if params[0] == 38 {
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 48 {
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.background(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.background(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.background(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 1 {
                // bold
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.bold(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.bold(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.bold(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 2 {
                // dim
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.dim(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.dim(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.dim(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 3 {
                // italic
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.italic(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.italic(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.italic(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 4 {
                // underline
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.underline(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.underline(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.underline(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 5 {
                // blink slow
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.blink_slow(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.blink_slow(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.blink_slow(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 6 {
                // blink fast
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.blink_fast(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.blink_fast(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.blink_fast(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 7 {
                // reverse
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.reverse(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.reverse(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.reverse(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 8 {
                // hidden
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.hidden(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.hidden(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.hidden(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 9 {
                // strike
                match (params.get(1), params.get(2)) {
                    (Some(param1), Some(param2)) => {
                        self.pending_styles = self.pending_styles.strike(Some(AnsiCode::Code((Some(*param1 as u16), Some(*param2 as u16)))));
                    },
                    (Some(param1), None) => {
                        self.pending_styles = self.pending_styles.strike(Some(AnsiCode::Code((Some(*param1 as u16), None))));
                    }
                    (_, _) => {
                        self.pending_styles = self.pending_styles.strike(Some(AnsiCode::Code((None, None))));
                    }
                };
            } else if params[0] == 30 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Black)));
            } else if params[0] == 31 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Red)));
            } else if params[0] == 32 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Green)));
            } else if params[0] == 33 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Yellow)));
            } else if params[0] == 34 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Blue)));
            } else if params[0] == 35 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Magenta)));
            } else if params[0] == 36 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::Cyan)));
            } else if params[0] == 37 {
                self.pending_styles = self.pending_styles.foreground(Some(AnsiCode::NamedColor(NamedColor::White)));
            } else if params[0] == 40 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Black)));
            } else if params[0] == 41 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Red)));
            } else if params[0] == 42 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Green)));
            } else if params[0] == 43 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Yellow)));
            } else if params[0] == 44 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Blue)));
            } else if params[0] == 45 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Magenta)));
            } else if params[0] == 46 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::Cyan)));
            } else if params[0] == 47 {
                self.pending_styles = self.pending_styles.background(Some(AnsiCode::NamedColor(NamedColor::White)));
            } else {
                debug_log_to_file(format!("unhandled csi m code {:?}", params), self.pid);
            }
        } else if c == 'C' { // move cursor forward
            let move_by = params[0] as usize;
            self.scroll.move_cursor_forward(move_by);
        } else if c == 'K' { // clear line (0 => right, 1 => left, 2 => all)
            if params[0] == 0 {
                self.scroll.clear_canonical_line_right_of_cursor();
            }
            // TODO: implement 1 and 2
        } else if c == 'J' { // clear all (0 => below, 1 => above, 2 => all, 3 => saved)
            if params[0] == 0 {
                self.scroll.clear_all_after_cursor();
            } else if params[0] == 2 {
                self.scroll.clear_all();
            }
            // TODO: implement 1
        } else if c == 'H' { // goto row/col
            let (row, col) = if params.len() == 1 {
                (params[0] as usize, 0) // TODO: is this always correct ?
            } else {
                (params[0] as usize - 1, params[1] as usize - 1) // we subtract 1 here because this csi is 1 indexed and we index from 0
            };
            self.scroll.move_cursor_to(row, col);
        } else if c == 'A' { // move cursor up until edge of screen
            let move_up_count = if params[0] == 0 { 1 } else { params[0] };
            self.scroll.move_cursor_up(move_up_count as usize);
        } else if c == 'D' {
            let move_back_count = if params[0] == 0 { 1 } else { params[0] as usize };
            self.scroll.move_cursor_back(move_back_count);
        } else if c == 'l' {
            // TBD
        } else if c == 'h' {
            // TBD
        } else if c == 'r' {
            if params.len() > 1 {
                let top_line_index = params[0] as usize;
                let bottom_line_index = params[1] as usize;
                self.scroll.set_scroll_region(top_line_index, bottom_line_index);
            } else {
                self.scroll.clear_scroll_region();
            }
        } else if c == 't' {
            // TBD - title?
        } else if c == 'n' {
            // TBD - device status report
        } else if c == 'c' {
            // TBD - identify terminal
        } else if c == 'M' {
            // delete lines if currently inside scroll region
            let line_count_to_delete = if params[0] == 0 { 1 } else { params[0] as usize };
            self.scroll.delete_lines_in_scroll_region(line_count_to_delete);
        } else if c == 'L' {
            // insert blank lines if inside scroll region
            let line_count_to_add = if params[0] == 0 { 1 } else { params[0] as usize };
            self.scroll.add_empty_lines_in_scroll_region(line_count_to_add);
        } else if c == 'q' || c == 'd' || c == 'X' || c == 'G' {
            // ignore for now to run on mac
        } else {
            panic!("unhandled csi: {:?}->{:?}", c, params);
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // TBD
    }
}
