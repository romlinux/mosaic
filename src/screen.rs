use std::io::Write;
use std::collections::{HashSet, BTreeMap};
use nix::pty::Winsize;
use std::os::unix::io::RawFd;
use std::sync::mpsc::{Sender, Receiver};

use crate::os_input_output::OsApi;
use crate::terminal_pane::TerminalPane;
use crate::pty_bus::{VteEvent, PtyInstruction};
use crate::boundaries::Boundaries;

/*
 * Screen
 *
 * this holds multiple panes (currently terminal panes) which are currently displayed on screen
 * it tracks their coordinates (x/y) and size, as well as how they should be resized
 *
 */

fn _debug_log_to_file (message: String) {
    use std::fs::OpenOptions;
    use std::io::prelude::*;
    let mut file = OpenOptions::new().append(true).create(true).open("/tmp/mosaic-log.txt").unwrap();
    file.write_all(message.as_bytes()).unwrap();
    file.write_all("\n".as_bytes()).unwrap();
}

const CURSOR_HEIGHT_WIDGH_RATIO: u16 = 4; // this is not accurate and kind of a magic number, TODO: look into this

type BorderAndPaneIds = (u16, Vec<RawFd>);

fn split_vertically_with_gap (rect: &Winsize) -> (Winsize, Winsize) {
    let width_of_each_half = (rect.ws_col - 1) / 2;
    let mut first_rect = rect.clone();
    let mut second_rect = rect.clone();
    if rect.ws_col % 2 == 0 {
        first_rect.ws_col = width_of_each_half + 1;
    } else {
        first_rect.ws_col = width_of_each_half;
    }
    second_rect.ws_col = width_of_each_half;
    (first_rect, second_rect)
}

fn split_horizontally_with_gap (rect: &Winsize) -> (Winsize, Winsize) {
    let height_of_each_half = (rect.ws_row - 1) / 2;
    let mut first_rect = rect.clone();
    let mut second_rect = rect.clone();
    if rect.ws_row % 2 == 0 {
        first_rect.ws_row = height_of_each_half + 1;
    } else {
        first_rect.ws_row = height_of_each_half;
    }
    second_rect.ws_row = height_of_each_half;
    (first_rect, second_rect)
}

#[derive(Debug)]
pub enum ScreenInstruction {
    Pty(RawFd, VteEvent),
    Render,
    NewPane(RawFd),
    HorizontalSplit(RawFd),
    VerticalSplit(RawFd),
    WriteCharacter(u8),
    ResizeLeft,
    ResizeRight,
    ResizeDown,
    ResizeUp,
    MoveFocus,
    Quit,
    ScrollUp,
    ScrollDown,
    ClearScroll,
    CloseFocusedPane,
    ClosePane(RawFd),
}

pub struct Screen {
    pub receiver: Receiver<ScreenInstruction>,
    max_panes: Option<usize>,
    send_pty_instructions: Sender<PtyInstruction>,
    full_screen_ws: Winsize,
    terminals: BTreeMap<RawFd, TerminalPane>, // BTreeMap because we need a predictable order when changing focus
    active_terminal: Option<RawFd>,
    os_api: Box<dyn OsApi>,
}

impl Screen {
    pub fn new (
        receive_screen_instructions: Receiver<ScreenInstruction>,
        send_pty_instructions: Sender<PtyInstruction>,
        full_screen_ws: &Winsize,
        os_api: Box<dyn OsApi>,
        max_panes: Option<usize>,
    ) -> Self {
        Screen {
            receiver: receive_screen_instructions,
            max_panes,
            send_pty_instructions,
            full_screen_ws: full_screen_ws.clone(),
            terminals: BTreeMap::new(),
            active_terminal: None,
            os_api,
        }
    }
    pub fn new_pane(&mut self, pid: RawFd) {
        self.close_down_to_max_terminals();
        if self.terminals.is_empty() {
            let x = 0;
            let y = 0;
            let new_terminal = TerminalPane::new(pid, self.full_screen_ws.clone(), x, y);
            self.os_api.set_terminal_size_using_fd(new_terminal.pid, new_terminal.display_cols, new_terminal.display_rows);
            self.terminals.insert(pid, new_terminal);
            self.active_terminal = Some(pid);
        } else {
            // TODO: check minimum size of active terminal

            let (_longest_edge, terminal_id_to_split) = self.terminals.iter().fold((0, 0), |(current_longest_edge, current_terminal_id_to_split), id_and_terminal_to_check| {
                let (id_of_terminal_to_check, terminal_to_check) = id_and_terminal_to_check;
                let terminal_size = (terminal_to_check.display_rows * CURSOR_HEIGHT_WIDGH_RATIO) * terminal_to_check.display_cols;
                if terminal_size > current_longest_edge {
                    (terminal_size, *id_of_terminal_to_check)
                } else {
                    (current_longest_edge, current_terminal_id_to_split)
                }
            });
            let terminal_to_split = self.terminals.get_mut(&terminal_id_to_split).unwrap();
            let terminal_ws = Winsize {
                ws_row: terminal_to_split.display_rows,
                ws_col: terminal_to_split.display_cols,
                ws_xpixel: terminal_to_split.x_coords,
                ws_ypixel: terminal_to_split.y_coords,
            };
            if terminal_to_split.display_rows * CURSOR_HEIGHT_WIDGH_RATIO > terminal_to_split.display_cols {
                let (top_winsize, bottom_winsize) = split_horizontally_with_gap(&terminal_ws);
                let bottom_half_y = terminal_ws.ws_ypixel + top_winsize.ws_row + 1;
                let new_terminal = TerminalPane::new(pid, bottom_winsize, terminal_ws.ws_xpixel, bottom_half_y);
                self.os_api.set_terminal_size_using_fd(new_terminal.pid, bottom_winsize.ws_col, bottom_winsize.ws_row);
                terminal_to_split.change_size(&top_winsize);
                self.terminals.insert(pid, new_terminal);
                self.os_api.set_terminal_size_using_fd(terminal_id_to_split, top_winsize.ws_col, top_winsize.ws_row);
                self.active_terminal = Some(pid);
            } else {
                let (left_winszie, right_winsize) = split_vertically_with_gap(&terminal_ws);
                let right_side_x = terminal_ws.ws_xpixel + left_winszie.ws_col + 1;
                let new_terminal = TerminalPane::new(pid, right_winsize, right_side_x, terminal_ws.ws_ypixel);
                self.os_api.set_terminal_size_using_fd(new_terminal.pid, right_winsize.ws_col, right_winsize.ws_row);
                terminal_to_split.change_size(&left_winszie);
                self.terminals.insert(pid, new_terminal);
                self.os_api.set_terminal_size_using_fd(terminal_id_to_split, left_winszie.ws_col, left_winszie.ws_row);
            }
            self.active_terminal = Some(pid);
            self.render();
        }
    }
    pub fn horizontal_split(&mut self, pid: RawFd) {
        self.close_down_to_max_terminals();
        if self.terminals.is_empty() {
            let x = 0;
            let y = 0;
            let new_terminal = TerminalPane::new(pid, self.full_screen_ws.clone(), x, y);
            self.os_api.set_terminal_size_using_fd(new_terminal.pid, new_terminal.display_cols, new_terminal.display_rows);
            self.terminals.insert(pid, new_terminal);
            self.active_terminal = Some(pid);
        } else {
            // TODO: check minimum size of active terminal
            let (active_terminal_ws, active_terminal_x_coords, active_terminal_y_coords) = {
                let active_terminal = &self.get_active_terminal().unwrap();
                (
                    Winsize {
                        ws_row: active_terminal.display_rows,
                        ws_col: active_terminal.display_cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    },
                    active_terminal.x_coords,
                    active_terminal.y_coords
                )
            };
            let (top_winsize, bottom_winsize) = split_horizontally_with_gap(&active_terminal_ws);
            let bottom_half_y = active_terminal_y_coords + top_winsize.ws_row + 1;
            let new_terminal = TerminalPane::new(pid, bottom_winsize, active_terminal_x_coords, bottom_half_y);
            self.os_api.set_terminal_size_using_fd(new_terminal.pid, bottom_winsize.ws_col, bottom_winsize.ws_row);

            {
                let active_terminal_id = &self.get_active_terminal_id().unwrap();
                let active_terminal = &mut self.terminals.get_mut(&active_terminal_id).unwrap();
                active_terminal.change_size(&top_winsize);
            }

            self.terminals.insert(pid, new_terminal);
            let active_terminal_pid = self.get_active_terminal_id().unwrap();
            self.os_api.set_terminal_size_using_fd(active_terminal_pid, top_winsize.ws_col, top_winsize.ws_row);
            self.active_terminal = Some(pid);
            self.render();
        }
    }
    pub fn vertical_split(&mut self, pid: RawFd) {
        self.close_down_to_max_terminals();
        if self.terminals.is_empty() {
            let x = 0;
            let y = 0;
            let new_terminal = TerminalPane::new(pid, self.full_screen_ws.clone(), x, y);
            self.os_api.set_terminal_size_using_fd(new_terminal.pid, new_terminal.display_cols, new_terminal.display_rows);
            self.terminals.insert(pid, new_terminal);
            self.active_terminal = Some(pid);
        } else {
            // TODO: check minimum size of active terminal
            let (active_terminal_ws, active_terminal_x_coords, active_terminal_y_coords) = {
                let active_terminal = &self.get_active_terminal().unwrap();
                (
                    Winsize {
                        ws_row: active_terminal.display_rows,
                        ws_col: active_terminal.display_cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    },
                    active_terminal.x_coords,
                    active_terminal.y_coords
                )
            };
            let (left_winszie, right_winsize) = split_vertically_with_gap(&active_terminal_ws);
            let right_side_x = active_terminal_x_coords + left_winszie.ws_col + 1;
            let new_terminal = TerminalPane::new(pid, right_winsize, right_side_x, active_terminal_y_coords);
            self.os_api.set_terminal_size_using_fd(new_terminal.pid, right_winsize.ws_col, right_winsize.ws_row);

            {
                let active_terminal_id = &self.get_active_terminal_id().unwrap();
                let active_terminal = &mut self.terminals.get_mut(&active_terminal_id).unwrap();
                active_terminal.change_size(&left_winszie);
            }

            self.terminals.insert(pid, new_terminal);
            let active_terminal_pid = self.get_active_terminal_id().unwrap();
            self.os_api.set_terminal_size_using_fd(active_terminal_pid, left_winszie.ws_col, left_winszie.ws_row);
            self.active_terminal = Some(pid);
            self.render();
        }
    }
    fn get_active_terminal (&self) -> Option<&TerminalPane> {
        match self.active_terminal {
            Some(active_terminal) => self.terminals.get(&active_terminal),
            None => None
        }
    }
    fn get_active_terminal_id (&self) -> Option<RawFd> {
        match self.active_terminal {
            Some(active_terminal) => Some(self.terminals.get(&active_terminal).unwrap().pid),
            None => None
        }
    }
    pub fn handle_pty_event(&mut self, pid: RawFd, event: VteEvent) {
        let terminal_output = self.terminals.get_mut(&pid).unwrap();
        terminal_output.handle_event(event);
    }
    pub fn write_to_active_terminal(&mut self, byte: u8) {
        if let Some(active_terminal_id) = &self.get_active_terminal_id() {
            let mut buffer = [byte];
            self.os_api.write_to_tty_stdin(*active_terminal_id, &mut buffer).expect("failed to write to terminal");
            self.os_api.tcdrain(*active_terminal_id).expect("failed to drain terminal");
        }
    }
    fn get_active_terminal_cursor_position(&self) -> (usize, usize) { // (x, y)
        let active_terminal = &self.get_active_terminal().unwrap();
        let (x_in_terminal, y_in_terminal) = active_terminal.cursor_coordinates();

        let x = active_terminal.x_coords as usize + x_in_terminal;
        let y = active_terminal.y_coords as usize + y_in_terminal;
        (x, y)
    }
    pub fn render (&mut self) {
        let mut stdout = self.os_api.get_stdout_writer();
        let mut boundaries = Boundaries::new(self.full_screen_ws.ws_col, self.full_screen_ws.ws_row);
        for (_pid, terminal) in self.terminals.iter_mut() {
            boundaries.add_rect(&terminal);
            if let Some(vte_output) = terminal.buffer_as_vte_output() {
                stdout.write_all(&vte_output.as_bytes()).expect("cannot write to stdout");
            }
        }

        // TODO: only render (and calculate) boundaries if there was a resize
        let vte_output = boundaries.vte_output();
        stdout.write_all(&vte_output.as_bytes()).expect("cannot write to stdout");

        let (cursor_position_x, cursor_position_y) = self.get_active_terminal_cursor_position();
        let goto_cursor_position = format!("\u{1b}[{};{}H\u{1b}[m", cursor_position_y + 1, cursor_position_x + 1); // goto row/col
        stdout.write_all(&goto_cursor_position.as_bytes()).expect("cannot write to stdout");
        stdout.flush().expect("could not flush");
    }
    fn terminal_ids_directly_left_of(&self, id: &RawFd) -> Option<Vec<RawFd>> {
        let mut ids = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        if terminal_to_check.x_coords == 0 {
            return None;
        }
        for (pid, terminal) in self.terminals.iter() {
            if terminal.x_coords + terminal.display_cols == terminal_to_check.x_coords - 1 {
                ids.push(*pid);
            }
        }
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    }
    fn terminal_ids_directly_right_of(&self, id: &RawFd) -> Option<Vec<RawFd>> {
        let mut ids = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        for (pid, terminal) in self.terminals.iter() {
            if terminal.x_coords == terminal_to_check.x_coords + terminal_to_check.display_cols + 1 {
                ids.push(*pid);
            }
        }
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    }
    fn terminal_ids_directly_below(&self, id: &RawFd) -> Option<Vec<RawFd>> {
        let mut ids = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        for (pid, terminal) in self.terminals.iter() {
            if terminal.y_coords == terminal_to_check.y_coords + terminal_to_check.display_rows + 1 {
                ids.push(*pid);
            }
        }
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    }
    fn terminal_ids_directly_above(&self, id: &RawFd) -> Option<Vec<RawFd>> {
        let mut ids = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        for (pid, terminal) in self.terminals.iter() {
            if terminal.y_coords + terminal.display_rows + 1 == terminal_to_check.y_coords {
                ids.push(*pid);
            }
        }
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    }
    fn panes_top_aligned_with_pane(&self, pane: &TerminalPane) -> Vec<&TerminalPane> {
        self.terminals
            .keys()
            .map(|t_id| self.terminals.get(&t_id).unwrap())
            .filter(|terminal| terminal.pid != pane.pid && terminal.y_coords == pane.y_coords)
            .collect()
    }
    fn panes_bottom_aligned_with_pane(&self, pane: &TerminalPane) -> Vec<&TerminalPane> {
        self.terminals
            .keys()
            .map(|t_id| self.terminals.get(&t_id).unwrap())
            .filter(|terminal| terminal.pid != pane.pid && terminal.y_coords + terminal.display_rows == pane.y_coords + pane.display_rows)
            .collect()
    }
    fn panes_right_aligned_with_pane(&self, pane: &TerminalPane) -> Vec<&TerminalPane> {
        self.terminals
            .keys()
            .map(|t_id| self.terminals.get(&t_id).unwrap())
            .filter(|terminal| terminal.pid != pane.pid && terminal.x_coords + terminal.display_cols == pane.x_coords + pane.display_cols)
            .collect()
    }
    fn panes_left_aligned_with_pane(&self, pane: &TerminalPane) -> Vec<&TerminalPane> {
        self.terminals
            .keys()
            .map(|t_id| self.terminals.get(&t_id).unwrap())
            .filter(|terminal| terminal.pid != pane.pid && terminal.x_coords == pane.x_coords)
            .collect()
    }
    fn right_aligned_contiguous_panes_above(&self, id: &RawFd, terminal_borders_to_the_right: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).expect("terminal id does not exist");
        let mut right_aligned_terminals = self.panes_right_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        right_aligned_terminals.sort_by(|a, b| { b.y_coords.cmp(&a.y_coords)});
        for terminal in right_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.y_coords + terminal.display_rows + 1 == terminal_to_check.y_coords {
                terminals.push(terminal);
            }
        }
        // top-most border aligned with a pane border to the right
        let mut top_resize_border = 0;
        for terminal in &terminals {
            let bottom_terminal_boundary = terminal.y_coords + terminal.display_rows;
            if terminal_borders_to_the_right.get(&(bottom_terminal_boundary + 1)).is_some() && top_resize_border < bottom_terminal_boundary {
                top_resize_border = bottom_terminal_boundary + 1;
            }
        }
        terminals.retain(|terminal| {
            terminal.y_coords >= top_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let top_resize_border = if terminals.is_empty() { terminal_to_check.y_coords } else { top_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (top_resize_border, terminal_ids)
    }
    fn right_aligned_contiguous_panes_below(&self, id: &RawFd, terminal_borders_to_the_right: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).expect("terminal id does not exist");
        let mut right_aligned_terminals = self.panes_right_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        right_aligned_terminals.sort_by(|a, b| { a.y_coords.cmp(&b.y_coords)});
        for terminal in right_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.y_coords == terminal_to_check.y_coords + terminal_to_check.display_rows + 1 {
                terminals.push(terminal);
            }
        }
        // bottom-most border aligned with a pane border to the right
        let mut bottom_resize_border = self.full_screen_ws.ws_row;
        for terminal in &terminals {
            let top_terminal_boundary = terminal.y_coords;
            if terminal_borders_to_the_right.get(&(top_terminal_boundary)).is_some() && top_terminal_boundary < bottom_resize_border {
                bottom_resize_border = top_terminal_boundary;
            }
        }
        terminals.retain(|terminal| {
            terminal.y_coords + terminal.display_rows <= bottom_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let bottom_resize_border = if terminals.is_empty() { terminal_to_check.y_coords + terminal_to_check.display_rows } else { bottom_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (bottom_resize_border, terminal_ids)
    }
    fn left_aligned_contiguous_panes_above(&self, id: &RawFd, terminal_borders_to_the_left: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).expect("terminal id does not exist");
        let mut left_aligned_terminals = self.panes_left_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        left_aligned_terminals.sort_by(|a, b| { b.y_coords.cmp(&a.y_coords)});
        for terminal in left_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.y_coords + terminal.display_rows + 1 == terminal_to_check.y_coords {
                terminals.push(terminal);
            }
        }
        // top-most border aligned with a pane border to the right
        let mut top_resize_border = 0;
        for terminal in &terminals {
            let bottom_terminal_boundary = terminal.y_coords + terminal.display_rows;
            if terminal_borders_to_the_left.get(&(bottom_terminal_boundary + 1)).is_some() && top_resize_border < bottom_terminal_boundary {
                top_resize_border = bottom_terminal_boundary + 1;
            }
        }
        terminals.retain(|terminal| {
            terminal.y_coords >= top_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let top_resize_border = if terminals.is_empty() { terminal_to_check.y_coords } else { top_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (top_resize_border, terminal_ids)
    }
    fn left_aligned_contiguous_panes_below(&self, id: &RawFd, terminal_borders_to_the_left: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).expect("terminal id does not exist");
        let mut left_aligned_terminals = self.panes_left_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        left_aligned_terminals.sort_by(|a, b| { a.y_coords.cmp(&b.y_coords)});
        for terminal in left_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.y_coords == terminal_to_check.y_coords + terminal_to_check.display_rows + 1 {
                terminals.push(terminal);
            }
        }
        // bottom-most border aligned with a pane border to the left
        let mut bottom_resize_border = self.full_screen_ws.ws_row;
        for terminal in &terminals {
            let top_terminal_boundary = terminal.y_coords;
            if terminal_borders_to_the_left.get(&(top_terminal_boundary)).is_some() && top_terminal_boundary < bottom_resize_border {
                bottom_resize_border = top_terminal_boundary;
            }
        }
        terminals.retain(|terminal| {
            // terminal.y_coords + terminal.display_rows < bottom_resize_border
            terminal.y_coords + terminal.display_rows <= bottom_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let bottom_resize_border = if terminals.is_empty() { terminal_to_check.y_coords + terminal_to_check.display_rows } else { bottom_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (bottom_resize_border, terminal_ids)
    }
    fn top_aligned_contiguous_panes_to_the_left(&self, id: &RawFd, terminal_borders_above: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).expect("terminal id does not exist");
        let mut top_aligned_terminals = self.panes_top_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        top_aligned_terminals.sort_by(|a, b| { b.x_coords.cmp(&a.x_coords)});
        for terminal in top_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.x_coords + terminal.display_cols + 1 == terminal_to_check.x_coords {
                terminals.push(terminal);
            }
        }
        // leftmost border aligned with a pane border above
        let mut left_resize_border = 0;
        for terminal in &terminals {
            let right_terminal_boundary = terminal.x_coords + terminal.display_cols;
            if terminal_borders_above.get(&(right_terminal_boundary + 1)).is_some() && left_resize_border < right_terminal_boundary {
                left_resize_border = right_terminal_boundary + 1;
            }
        }
        terminals.retain(|terminal| {
            terminal.x_coords >= left_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let left_resize_border = if terminals.is_empty() { terminal_to_check.x_coords } else { left_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (left_resize_border, terminal_ids)
    }
    fn top_aligned_contiguous_panes_to_the_right(&self, id: &RawFd, terminal_borders_above: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        let mut top_aligned_terminals = self.panes_top_aligned_with_pane(&terminal_to_check);
        // terminals that are next to each other up to current
        top_aligned_terminals.sort_by(|a, b| { a.x_coords.cmp(&b.x_coords)});
        for terminal in top_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.x_coords == terminal_to_check.x_coords + terminal_to_check.display_cols + 1 {
                terminals.push(terminal);
            }
        }
        // rightmost border aligned with a pane border above
        let mut right_resize_border = self.full_screen_ws.ws_col;
        for terminal in &terminals {

            let left_terminal_boundary = terminal.x_coords;
            if terminal_borders_above.get(&left_terminal_boundary).is_some() && right_resize_border > left_terminal_boundary {
                right_resize_border = left_terminal_boundary;
            }
        }
        terminals.retain(|terminal| {
            terminal.x_coords + terminal.display_cols <= right_resize_border 
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let right_resize_border = if terminals.is_empty() { terminal_to_check.x_coords + terminal_to_check.display_cols } else { right_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (right_resize_border, terminal_ids)
    }
    fn bottom_aligned_contiguous_panes_to_the_left(&self, id: &RawFd, terminal_borders_below: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        let mut bottom_aligned_terminals = self.panes_bottom_aligned_with_pane(&terminal_to_check);
        bottom_aligned_terminals.sort_by(|a, b| { b.x_coords.cmp(&a.x_coords)});
        // terminals that are next to each other up to current
        for terminal in bottom_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.x_coords + terminal.display_cols + 1 == terminal_to_check.x_coords {
                terminals.push(terminal);
            }
        }
        // leftmost border aligned with a pane border above
        let mut left_resize_border = 0;
        for terminal in &terminals {
            let right_terminal_boundary = terminal.x_coords + terminal.display_cols;
            if terminal_borders_below.get(&(right_terminal_boundary + 1)).is_some() && left_resize_border < right_terminal_boundary {
                left_resize_border = right_terminal_boundary + 1;
            }
        }
        terminals.retain(|terminal| {
            terminal.x_coords >= left_resize_border
        });
        // if there are no adjacent panes to resize, we use the border of the main pane we're
        // resizing
        let left_resize_border = if terminals.is_empty() { terminal_to_check.x_coords } else { left_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (left_resize_border, terminal_ids)
    }
    fn bottom_aligned_contiguous_panes_to_the_right(&self, id: &RawFd, terminal_borders_below: &HashSet<u16>) -> BorderAndPaneIds {
        let mut terminals = vec![];
        let terminal_to_check = self.terminals.get(id).unwrap();
        let mut bottom_aligned_terminals = self.panes_bottom_aligned_with_pane(&terminal_to_check);
        bottom_aligned_terminals.sort_by(|a, b| { a.x_coords.cmp(&b.x_coords)});
        // terminals that are next to each other up to current
        for terminal in bottom_aligned_terminals {
            let terminal_to_check = terminals
                .last()
                .unwrap_or(&terminal_to_check);
            if terminal.x_coords == terminal_to_check.x_coords + terminal_to_check.display_cols + 1 {
                terminals.push(terminal);
            }
        }
        // leftmost border aligned with a pane border above
        let mut right_resize_border = self.full_screen_ws.ws_col;
        for terminal in &terminals {
            let left_terminal_boundary = terminal.x_coords;
            if terminal_borders_below.get(&left_terminal_boundary).is_some() && right_resize_border > left_terminal_boundary {
                right_resize_border = left_terminal_boundary;
            }
        }
        terminals.retain(|terminal| {
            terminal.x_coords + terminal.display_cols <= right_resize_border 
        });
        let right_resize_border = if terminals.is_empty() { terminal_to_check.x_coords + terminal_to_check.display_cols } else { right_resize_border };
        let terminal_ids: Vec<RawFd> = terminals.iter().map(|t| t.pid).collect();
        (right_resize_border, terminal_ids)
    }
    fn reduce_pane_height_down(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(id).unwrap();
        terminal.reduce_height_down(count);
        self.os_api.set_terminal_size_using_fd(
            *id,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn reduce_pane_height_up(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(id).unwrap();
        terminal.reduce_height_up(count);
        self.os_api.set_terminal_size_using_fd(
            *id,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn increase_pane_height_down(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.increase_height_down(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn increase_pane_height_up(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.increase_height_up(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn increase_pane_width_right(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.increase_width_right(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn increase_pane_width_left(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.increase_width_left(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn reduce_pane_width_right(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.reduce_width_right(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn reduce_pane_width_left(&mut self, id: &RawFd, count: u16) {
        let terminal = self.terminals.get_mut(&id).unwrap();
        terminal.reduce_width_left(count);
        self.os_api.set_terminal_size_using_fd(
            terminal.pid,
            terminal.display_cols,
            terminal.display_rows
        );
    }
    fn pane_is_between_vertical_borders(&self, id: &RawFd, left_border_x: u16, right_border_x: u16) -> bool {
        let terminal = self.terminals.get(id).expect("could not find terminal to check between borders");
        terminal.x_coords >= left_border_x && terminal.x_coords + terminal.display_cols <= right_border_x
    }
    fn pane_is_between_horizontal_borders(&self, id: &RawFd, top_border_y: u16, bottom_border_y: u16) -> bool {
        let terminal = self.terminals.get(id).expect("could not find terminal to check between borders");
        terminal.y_coords >= top_border_y && terminal.y_coords + terminal.display_rows <= bottom_border_y
    }
    fn reduce_pane_and_surroundings_up(&mut self, id: &RawFd, count: u16) {
        let mut terminals_below = self.terminal_ids_directly_below(&id).expect("can't reduce pane size up if there are no terminals below");
        let terminal_borders_below: HashSet<u16> = terminals_below.iter().map(|t| self.terminals.get(t).unwrap().x_coords).collect();
        let (left_resize_border, terminals_to_the_left) = self.bottom_aligned_contiguous_panes_to_the_left(&id, &terminal_borders_below);
        let (right_resize_border, terminals_to_the_right) = self.bottom_aligned_contiguous_panes_to_the_right(&id, &terminal_borders_below);
        terminals_below.retain(|t| self.pane_is_between_vertical_borders(t, left_resize_border, right_resize_border));
        self.reduce_pane_height_up(&id, count);
        for terminal_id in terminals_below {
            self.increase_pane_height_up(&terminal_id, count);
        }
        for terminal_id in terminals_to_the_left.iter().chain(terminals_to_the_right.iter()) {
            self.reduce_pane_height_up(&terminal_id, count);
        }
    }
    fn reduce_pane_and_surroundings_down(&mut self, id: &RawFd, count: u16) {
        let mut terminals_above = self.terminal_ids_directly_above(&id).expect("can't reduce pane size down if there are no terminals above");
        let terminal_borders_above: HashSet<u16> = terminals_above.iter().map(|t| self.terminals.get(t).unwrap().x_coords).collect();
        let (left_resize_border, terminals_to_the_left) = self.top_aligned_contiguous_panes_to_the_left(&id, &terminal_borders_above);
        let (right_resize_border, terminals_to_the_right) = self.top_aligned_contiguous_panes_to_the_right(&id, &terminal_borders_above);
        terminals_above.retain(|t| self.pane_is_between_vertical_borders(t, left_resize_border, right_resize_border));
        self.reduce_pane_height_down(&id, count);
        for terminal_id in terminals_above {
            self.increase_pane_height_down(&terminal_id, count);
        }
        for terminal_id in terminals_to_the_left.iter().chain(terminals_to_the_right.iter()) {
            self.reduce_pane_height_down(&terminal_id, count);
        }
    }
    fn reduce_pane_and_surroundings_right(&mut self, id: &RawFd, count: u16) {
        let mut terminals_to_the_left = self.terminal_ids_directly_left_of(&id).expect("can't reduce pane size right if there are no terminals to the left");
        let terminal_borders_to_the_left: HashSet<u16> = terminals_to_the_left.iter().map(|t| self.terminals.get(t).unwrap().y_coords).collect();
        let (top_resize_border, terminals_above) = self.left_aligned_contiguous_panes_above(&id, &terminal_borders_to_the_left);
        let (bottom_resize_border, terminals_below) = self.left_aligned_contiguous_panes_below(&id, &terminal_borders_to_the_left);
        terminals_to_the_left.retain(|t| self.pane_is_between_horizontal_borders(t, top_resize_border, bottom_resize_border));
        self.reduce_pane_width_right(&id, count);
        for terminal_id in terminals_to_the_left {
            self.increase_pane_width_right(&terminal_id, count);
        }
        for terminal_id in terminals_above.iter().chain(terminals_below.iter()) {
            self.reduce_pane_width_right(&terminal_id, count);
        }
    }
    fn reduce_pane_and_surroundings_left(&mut self, id: &RawFd, count: u16) {
        let mut terminals_to_the_right = self.terminal_ids_directly_right_of(&id).expect("can't reduce pane size left if there are no terminals to the right");
        let terminal_borders_to_the_right: HashSet<u16> = terminals_to_the_right.iter().map(|t| self.terminals.get(t).unwrap().y_coords).collect();
        let (top_resize_border, terminals_above) = self.right_aligned_contiguous_panes_above(&id, &terminal_borders_to_the_right);
        let (bottom_resize_border, terminals_below) = self.right_aligned_contiguous_panes_below(&id, &terminal_borders_to_the_right);
        terminals_to_the_right.retain(|t| self.pane_is_between_horizontal_borders(t, top_resize_border, bottom_resize_border));
        self.reduce_pane_width_left(&id, count);
        for terminal_id in terminals_to_the_right {
            self.increase_pane_width_left(&terminal_id, count);
        }
        for terminal_id in terminals_above.iter().chain(terminals_below.iter()) {
            self.reduce_pane_width_left(&terminal_id, count);
        }
    }
    fn increase_pane_and_surroundings_up(&mut self, id: &RawFd, count: u16) {
        let mut terminals_above = self.terminal_ids_directly_above(&id).expect("can't increase pane size up if there are no terminals above");
        let terminal_borders_above: HashSet<u16> = terminals_above.iter().map(|t| self.terminals.get(t).unwrap().x_coords).collect();
        let (left_resize_border, terminals_to_the_left) = self.top_aligned_contiguous_panes_to_the_left(&id, &terminal_borders_above);
        let (right_resize_border, terminals_to_the_right) = self.top_aligned_contiguous_panes_to_the_right(&id, &terminal_borders_above);
        terminals_above.retain(|t| self.pane_is_between_vertical_borders(t, left_resize_border, right_resize_border));
        self.increase_pane_height_up(&id, count);
        for terminal_id in terminals_above {
            self.reduce_pane_height_up(&terminal_id, count);
        }
        for terminal_id in terminals_to_the_left.iter().chain(terminals_to_the_right.iter()) {
            self.increase_pane_height_up(&terminal_id, count);
        }
    }
    fn increase_pane_and_surroundings_down(&mut self, id: &RawFd, count: u16) {
        let mut terminals_below = self.terminal_ids_directly_below(&id).expect("can't increase pane size down if there are no terminals below");
        let terminal_borders_below: HashSet<u16> = terminals_below.iter().map(|t| self.terminals.get(t).unwrap().x_coords).collect();
        let (left_resize_border, terminals_to_the_left) = self.bottom_aligned_contiguous_panes_to_the_left(&id, &terminal_borders_below);
        let (right_resize_border, terminals_to_the_right) = self.bottom_aligned_contiguous_panes_to_the_right(&id, &terminal_borders_below);
        terminals_below.retain(|t| self.pane_is_between_vertical_borders(t, left_resize_border, right_resize_border));
        self.increase_pane_height_down(&id, count);
        for terminal_id in terminals_below {
            self.reduce_pane_height_down(&terminal_id, count);
        }
        for terminal_id in terminals_to_the_left.iter().chain(terminals_to_the_right.iter()) {
            self.increase_pane_height_down(&terminal_id, count);
        }
    }
    fn increase_pane_and_surroundings_right(&mut self, id: &RawFd, count: u16) {
        let mut terminals_to_the_right = self.terminal_ids_directly_right_of(&id).expect("can't increase pane size right if there are no terminals to the right");
        let terminal_borders_to_the_right: HashSet<u16> = terminals_to_the_right.iter().map(|t| self.terminals.get(t).unwrap().y_coords).collect();
        let (top_resize_border, terminals_above) = self.right_aligned_contiguous_panes_above(&id, &terminal_borders_to_the_right);
        let (bottom_resize_border, terminals_below) = self.right_aligned_contiguous_panes_below(&id, &terminal_borders_to_the_right);
        terminals_to_the_right.retain(|t| self.pane_is_between_horizontal_borders(t, top_resize_border, bottom_resize_border));
        self.increase_pane_width_right(&id, count);
        for terminal_id in terminals_to_the_right {
            self.reduce_pane_width_right(&terminal_id, count);
        }
        for terminal_id in terminals_above.iter().chain(terminals_below.iter()) {
            self.increase_pane_width_right(&terminal_id, count);
        }
    }
    fn increase_pane_and_surroundings_left(&mut self, id: &RawFd, count: u16) {
        let mut terminals_to_the_left = self.terminal_ids_directly_left_of(&id).expect("can't increase pane size right if there are no terminals to the right");
        let terminal_borders_to_the_left: HashSet<u16> = terminals_to_the_left.iter().map(|t| self.terminals.get(t).unwrap().y_coords).collect();
        let (top_resize_border, terminals_above) = self.left_aligned_contiguous_panes_above(&id, &terminal_borders_to_the_left);
        let (bottom_resize_border, terminals_below) = self.left_aligned_contiguous_panes_below(&id, &terminal_borders_to_the_left);
        terminals_to_the_left.retain(|t| self.pane_is_between_horizontal_borders(t, top_resize_border, bottom_resize_border));
        self.increase_pane_width_left(&id, count);
        for terminal_id in terminals_to_the_left {
            self.reduce_pane_width_left(&terminal_id, count);
        }
        for terminal_id in terminals_above.iter().chain(terminals_below.iter()) {
            self.increase_pane_width_left(&terminal_id, count);
        }
    }
    fn panes_exist_above(&self, pane_id: &RawFd) -> bool {
        let pane = self.terminals.get(pane_id).expect("pane does not exist");
        pane.y_coords > 0
    }
    fn panes_exist_below(&self, pane_id: &RawFd) -> bool {
        let pane = self.terminals.get(pane_id).expect("pane does not exist");
        pane.y_coords + pane.display_rows < self.full_screen_ws.ws_row
    }
    fn panes_exist_to_the_right(&self, pane_id: &RawFd) -> bool {
        let pane = self.terminals.get(pane_id).expect("pane does not exist");
        pane.x_coords + pane.display_cols < self.full_screen_ws.ws_col
    }
    fn panes_exist_to_the_left(&self, pane_id: &RawFd) -> bool {
        let pane = self.terminals.get(pane_id).expect("pane does not exist");
        pane.x_coords > 0
    }
    pub fn resize_right (&mut self) {
        // TODO: find out by how much we actually reduced and only reduce by that much
        let count = 10;
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            if self.panes_exist_to_the_right(&active_terminal_id) {
                self.increase_pane_and_surroundings_right(&active_terminal_id, count);
                self.render();
            } else if self.panes_exist_to_the_left(&active_terminal_id) {
                self.reduce_pane_and_surroundings_right(&active_terminal_id, count);
                self.render();
            }
        }
    }
    pub fn resize_left (&mut self) {
        // TODO: find out by how much we actually reduced and only reduce by that much
        let count = 10;
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            if self.panes_exist_to_the_right(&active_terminal_id) {
                self.reduce_pane_and_surroundings_left(&active_terminal_id, count);
                self.render();
            } else if self.panes_exist_to_the_left(&active_terminal_id) {
                self.increase_pane_and_surroundings_left(&active_terminal_id, count);
                self.render();
            }
        }
    }
    pub fn resize_down (&mut self) {
        // TODO: find out by how much we actually reduced and only reduce by that much
        let count = 2;
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            if self.panes_exist_above(&active_terminal_id) {
                self.reduce_pane_and_surroundings_down(&active_terminal_id, count);
                self.render();
            } else if self.panes_exist_below(&active_terminal_id) {
                self.increase_pane_and_surroundings_down(&active_terminal_id, count);
                self.render();
            }
        }
    }
    pub fn resize_up (&mut self) {
        // TODO: find out by how much we actually reduced and only reduce by that much
        let count = 2;
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            if self.panes_exist_above(&active_terminal_id) {
                self.increase_pane_and_surroundings_up(&active_terminal_id, count);
                self.render();
            } else if self.panes_exist_below(&active_terminal_id) {
                self.reduce_pane_and_surroundings_up(&active_terminal_id, count);
                self.render();
            }
        }
    }
    pub fn move_focus(&mut self) {
        if self.terminals.is_empty() {
            return;
        }
        let active_terminal_id = self.get_active_terminal_id().unwrap();
        let terminal_ids: Vec<RawFd> = self.terminals.keys().copied().collect(); // TODO: better, no allocations
        let first_terminal = terminal_ids.get(0).unwrap();
        let active_terminal_id_position = terminal_ids.iter().position(|id| id == &active_terminal_id).unwrap();
        if let Some(next_terminal) = terminal_ids.get(active_terminal_id_position + 1) {
            self.active_terminal = Some(*next_terminal);
        } else {
            self.active_terminal = Some(*first_terminal);
        }
        self.render();
    }
    fn horizontal_borders(&self, terminals: &[RawFd]) -> HashSet<u16> {
        terminals.iter().fold(HashSet::new(), |mut borders, t| {
            let terminal = self.terminals.get(t).unwrap();
            borders.insert(terminal.y_coords);
            borders.insert(terminal.y_coords + terminal.display_rows + 1); // 1 for the border width
            borders
        })
    }
    fn vertical_borders(&self, terminals: &[RawFd]) -> HashSet<u16> {
        terminals.iter().fold(HashSet::new(), |mut borders, t| {
            let terminal = self.terminals.get(t).unwrap();
            borders.insert(terminal.x_coords);
            borders.insert(terminal.x_coords + terminal.display_cols + 1); // 1 for the border width
            borders
        })
    }
    fn terminals_to_the_left_between_aligning_borders(&self, id: RawFd) -> Option<Vec<RawFd>> {
        if let Some(terminal) = &self.terminals.get(&id) {
            let upper_close_border = terminal.y_coords;
            let lower_close_border = terminal.y_coords + terminal.display_rows + 1;

            if let Some(mut terminals_to_the_left) = self.terminal_ids_directly_left_of(&id) {
                let terminal_borders_to_the_left = self.horizontal_borders(&terminals_to_the_left);
                if terminal_borders_to_the_left.contains(&upper_close_border) && terminal_borders_to_the_left.contains(&lower_close_border) {
                    terminals_to_the_left.retain(|t| self.pane_is_between_horizontal_borders(t, upper_close_border, lower_close_border));
                    return Some(terminals_to_the_left);
                }
            }
        }
        None
    }
    fn terminals_to_the_right_between_aligning_borders(&self, id: RawFd) -> Option<Vec<RawFd>> {
        if let Some(terminal) = &self.terminals.get(&id) {
            let upper_close_border = terminal.y_coords;
            let lower_close_border = terminal.y_coords + terminal.display_rows + 1;

            if let Some(mut terminals_to_the_right) = self.terminal_ids_directly_right_of(&id) {
                let terminal_borders_to_the_right = self.horizontal_borders(&terminals_to_the_right);
                if terminal_borders_to_the_right.contains(&upper_close_border) && terminal_borders_to_the_right.contains(&lower_close_border) {
                    terminals_to_the_right.retain(|t| self.pane_is_between_horizontal_borders(t, upper_close_border, lower_close_border));
                    return Some(terminals_to_the_right);
                }
            }
        }
        None
    }
    fn terminals_above_between_aligning_borders(&self, id: RawFd) -> Option<Vec<RawFd>> {
        if let Some(terminal) = &self.terminals.get(&id) {
            let left_close_border = terminal.x_coords;
            let right_close_border = terminal.x_coords + terminal.display_cols + 1;

            if let Some(mut terminals_above) = self.terminal_ids_directly_above(&id) {
                let terminal_borders_above = self.vertical_borders(&terminals_above);
                if terminal_borders_above.contains(&left_close_border) && terminal_borders_above.contains(&right_close_border) {
                    terminals_above.retain(|t| self.pane_is_between_vertical_borders(t, left_close_border, right_close_border));
                    return Some(terminals_above);
                }
            }
        }
        None
    }
    fn terminals_below_between_aligning_borders(&self, id: RawFd) -> Option<Vec<RawFd>> {
        if let Some(terminal) = &self.terminals.get(&id) {
            let left_close_border = terminal.x_coords;
            let right_close_border = terminal.x_coords + terminal.display_cols + 1;

            if let Some(mut terminals_below) = self.terminal_ids_directly_below(&id) {
                let terminal_borders_below = self.vertical_borders(&terminals_below);
                if terminal_borders_below.contains(&left_close_border) && terminal_borders_below.contains(&right_close_border) {
                    terminals_below.retain(|t| self.pane_is_between_vertical_borders(t, left_close_border, right_close_border));
                    return Some(terminals_below);
                }
            }
        }
        None
    }
    fn close_down_to_max_terminals (&mut self) {
        if let Some(max_panes) = self.max_panes {
            if self.terminals.len() >= max_panes {
                for _ in max_panes..=self.terminals.len() {
                    let first_pid = *self.terminals.iter().next().unwrap().0;
                    self.send_pty_instructions.send(PtyInstruction::ClosePane(first_pid)).unwrap();
                    self.close_pane_without_rerender(first_pid); // TODO: do not render yet
                }
            }
        }
    }
    pub fn close_pane(&mut self, id: RawFd) {
        if self.terminals.get(&id).is_some() {
            self.close_pane_without_rerender(id);
            self.render();
        }
    }
    pub fn close_pane_without_rerender(&mut self, id: RawFd) {
        if let Some(terminal_to_close) = &self.terminals.get(&id) {
            let terminal_to_close_width = terminal_to_close.display_cols;
            let terminal_to_close_height = terminal_to_close.display_rows;
            if let Some(terminals) = self.terminals_to_the_left_between_aligning_borders(id) {
                for terminal_id in terminals.iter() {
                    &self.increase_pane_width_right(&terminal_id, terminal_to_close_width + 1); // 1 for the border
                }
                if self.active_terminal == Some(id) {
                    self.active_terminal = Some(*terminals.last().unwrap());
                }
            } else if let Some(terminals) = self.terminals_to_the_right_between_aligning_borders(id) {
                for terminal_id in terminals.iter() {
                    &self.increase_pane_width_left(&terminal_id, terminal_to_close_width + 1); // 1 for the border
                }
                if self.active_terminal == Some(id) {
                    self.active_terminal = Some(*terminals.last().unwrap());
                }
            } else if let Some(terminals) = self.terminals_above_between_aligning_borders(id) {
                for terminal_id in terminals.iter() {
                    &self.increase_pane_height_down(&terminal_id, terminal_to_close_height + 1); // 1 for the border
                }
                if self.active_terminal == Some(id) {
                    self.active_terminal = Some(*terminals.last().unwrap());
                }
            } else if let Some(terminals) = self.terminals_below_between_aligning_borders(id) {
                for terminal_id in terminals.iter() {
                    &self.increase_pane_height_up(&terminal_id, terminal_to_close_height + 1); // 1 for the border
                }
                if self.active_terminal == Some(id) {
                    self.active_terminal = Some(*terminals.last().unwrap());
                }
            } else {
                return; // TODO: exit app? here we're trying to close the last pane on screen
            }
            self.terminals.remove(&id);
        }
    }
    pub fn close_focused_pane(&mut self) {
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            self.send_pty_instructions.send(PtyInstruction::ClosePane(active_terminal_id)).unwrap();
            self.close_pane(active_terminal_id);
        }
    }
    pub fn scroll_active_terminal_up(&mut self) {
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            let active_terminal = self.terminals.get_mut(&active_terminal_id).unwrap();
            active_terminal.scroll_up(1);
            self.render();
        }
    }
    pub fn scroll_active_terminal_down(&mut self) {
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            let active_terminal = self.terminals.get_mut(&active_terminal_id).unwrap();
            active_terminal.scroll_down(1);
            self.render();
        }
    }
    pub fn clear_active_terminal_scroll(&mut self) {
        if let Some(active_terminal_id) = self.get_active_terminal_id() {
            let active_terminal = self.terminals.get_mut(&active_terminal_id).unwrap();
            active_terminal.clear_scroll();
        }
    }
}
