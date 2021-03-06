extern crate nix;

use std::io;
use std::io::prelude::*;
use std::io::Write;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::env::args;
use std::path::Path;
use std::cmp;
use std::time::{Duration, Instant};

use nix::sys::termios;

pub struct Config {
    tab_width: i32,
}

/// A data type that represents where in the console window something resides.
/// Indexing starts at 0 (even though the VT100 escape sequences expect
/// coordinates starting at 1), because mixing 1-based indexing with 0-based
/// indexing can lead to errors. Pos { col: 0, row: 0 } corresponds to the top left
/// corner of the terminal.
#[derive(Debug, Clone, Copy)]
struct Pos {
    col: usize,
    row: usize,
}

enum Key {
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    PageUp,
    PageDown,
    LineHome,
    LineEnd,
    FileHome,
    FileEnd,
    Delete,
}

fn ctrl_mask(c: char) -> char {
    (c as u8 & 0x1f) as char
}

#[derive(Debug)]
struct Cursor {
    /// The position of the cursor in the terminal window.
    pos: Pos,
    /// Since lines may take up several rows, the specific line with the cursor
    /// cannot simply be calculated with `pos`, so the index of the line in the
    /// lines list needs to be stored.
    line: usize,
    /// To the same reason as above, there is no way to retrieve the actual
    /// byte in line under cursor, so the absolute offset from the line's start
    /// needs to be stored here.
    byte: usize,
    /// In order to be able to go up and down along the ends of lines of
    /// different lengths (including 0), this flag needs to be set to determine
    /// whether to go to the same column in the next row or to its end.
    // TODO don't limit to EoL: make it universal, as in with a `stay_on_col` field
    is_at_eol: bool,
}

struct Line {
    // The original representation of the line.
    orig: Vec<u8>,
    // Represents how the line is rendered on screen.
    render: Vec<u8>
}

impl Line {
    fn len(&self) -> usize {
        self.render.len()
    }

    fn is_empty(&self) -> bool {
        self.render.is_empty()
    }
}

struct StatusMsg {
    data: String,
    // The time the status message was issued. All status messages remain on the
    // screen for at least `timeout` seconds.
    timestamp: Instant,
    timeout: Duration,
}

struct Editor {
    // Note that this does not always report the actual position of the cursor.
    // Instead, it reflects the _desired_ position, i.e. what user sets. It may
    // be that for rendering purposes the cursor is temporarily relocated but
    // then set back to this position. This also means that when it's
    // temporarily relocated, this field shall not be updated.
    cursor: Cursor,
    window_width: usize,
    window_height: usize,
    // Used to coalesce writes into a single buffer to then flush it in one go
    // to avoid excessive IO overhead.
    write_buf: Vec<u8>,
    // Note that there is a distinction between rows and lines. A line is the
    // string of text until the new-line character, as stored in the file, while
    // a row is the rendered string that fits into a single row in the window.
    // Thus a line may wrap several rows.
    lines: Vec<Line>,
    // The zero-based index into `lines` of the first line to show.
    line_offset: usize,
    // The first character of the row in line that should be drawn. Always
    // a multiple of `window_width`. Also zero-based.
    line_offset_byte: usize,
    config: Config,
    // The path of the file currently being edited. Stored as a string since
    // we're only printing it on the status bar.
    path: String,
    // Store the status message so that it's persisted across screen redraws.
    status_msg: StatusMsg,
}

impl Editor {
    fn new(config: Config, path: String) -> Editor {
        Editor {
            cursor: Cursor { pos: Pos { row: 0, col: 0 }, line: 0, byte: 0, is_at_eol: false },
            window_width: 0,
            window_height: 0,
            write_buf: vec![],
            lines: vec![],
            line_offset: 0,
            line_offset_byte: 0,
            config: config,
            path: path,
            status_msg: StatusMsg {
                data: String::new(),
                timestamp: Instant::now(),
                timeout: Duration::new(0, 0),
            },
        }
    }

    pub fn open_file(config: Config, path: &Path) -> std::io::Result<Editor> {
        let mut file = File::open(path)?;
        let path = path.file_name().unwrap().to_str().unwrap().to_string();
        let mut editor = Editor::new(config, path);
        let mut buf = vec![];

        file.read_to_end(&mut buf).unwrap();

        // TODO might need to match \r\n as well
        // FIXME there's an extra empty space at the end even if there shouldn't be
        let lines = buf.split(|b| *b == '\n' as u8);

        // Try to get an esimate of the number of lines in file.
        let size_hint = {
            let (lower, upper) = lines.size_hint();
            if let Some(upper) = upper { upper } else { lower }
        };

        if size_hint > 0 {
            editor.lines.reserve(size_hint);
        }

        editor.lines = lines
            .map(|line| Line {
                orig: line.to_vec(),
                render: editor.line_orig_to_render(&line)
            })
            .collect();

        let dbg_lines: Vec<String> = editor.lines.iter()
            .map(|line| String::from_utf8_lossy(&line.orig).to_string())
            .collect();
        log(format!("file ({} lines):\n{:?}", editor.lines.len(), dbg_lines).as_bytes());

        Ok(editor)
    }

    pub fn run(&mut self) {
        let mut buf: [u8; 1] = [0; 1];
        self.refresh_screen();
        self.new_status_msg("HELP: Ctrl-C to quit", Duration::from_secs(5));
        loop {
            self.refresh_screen();
            // TODO is there a canonical way of getting a single byte from stdin?
            if let Ok(_) = io::stdin().read_exact(&mut buf) {
                let b = buf[0];
                if b as char == ctrl_mask('c') {
                    break;
                } else {
                    self.handle_key(b as char)
                }
            } else {
                break;
            }
        }
    }

    fn handle_key(&mut self, c: char) {
        match c {
            '\x1b' => self.handle_esc_seq_key(),
            _ => self.handle_input(c)
        }
    }

    fn handle_esc_seq_key(&mut self) {
        if let Some(key) = self.read_esc_seq_to_key() {
            match key {
                Key::ArrowUp => self.cursor_up(),
                Key::ArrowDown => self.cursor_down(),
                Key::ArrowLeft => self.cursor_left(),
                Key::ArrowRight => self.cursor_right(),
                Key::PageUp => self.page_up(),
                Key::PageDown => self.page_down(),
                Key::LineHome => {
                    while self.cursor.byte > 0 {
                        self.cursor_left();
                    }
                },
                // FIXME this doesn't work
                Key::LineEnd => {
                    while self.cursor.byte + 1 < self.lines[self.cursor.line].len()
                        && self.cursor.pos.col + 1 < self.window_width {
                        self.cursor_right();
                    }
                },
                Key::FileHome => {
                }
                Key::FileEnd => {
                }
                _ => (),
            }
        }
    }

    fn page_down(&mut self) {
        //let lines_left = self.lines.len() - self.cursor.line;
        //let at_least_n_rows = cmp::min(self.window_height, lines_left);
        let mut n_rows_left = self.window_height - 1;
        while n_rows_left > 0 && self.cursor.line < self.lines.len() {
            self.cursor_down();
            n_rows_left -= 1;
        }
    }

    fn page_up(&mut self) {
        let mut n_rows_left = self.window_height - 1;
        //let n_rows = cmp::min(self.window_height, self.cursor.pos.row);
        while n_rows_left > 0 && self.cursor.line > 0 {
            self.cursor_up();
            n_rows_left -= 1;
        }
    }

    /// Moves the cursor down by one row, if possible.
    fn cursor_down(&mut self) {
        // Check if cursor is at the bottom of the window.
        if self.cursor.pos.row + 1 == self.window_height {
            self.scroll_down();
        }

        // Note that this is indexed from the beginning of the line, whereas
        // curr_last_pos_row_offset is indexed from the beginning of the row.
        let next_rows_len = self.curr_line_next_rows_len();

        log(format!("DOWN: cursor: {:?}, row_last_byte: {}, next_rows_len: {}, line_offset: {}, line_offset_byte: {}, line.len: {}",
                    self.cursor, self.curr_last_pos_line_offset(), next_rows_len, self.line_offset,
                    self.line_offset_byte, self.lines[self.cursor.line].len()).as_bytes());

        if next_rows_len > 0 {
            // We're not at the end of the line, which is merely wrapped, so
            // just go down one row staying on the same line.
            if self.cursor.pos.row + 1 < self.window_height {
                self.cursor.pos.row += 1;
            }

            let next_row_len = cmp::min(next_rows_len, self.window_width);
            let col = {
                if self.cursor.is_at_eol {
                    next_row_len - 1
                } else {
                    cmp::min(self.cursor.pos.col, next_row_len - 1)
                }
            };

            log(format!("DOWN|wrap: next_row_len: {}, col: {}", next_row_len, col).as_bytes());

            self.cursor.pos.col = col;
            self.cursor.byte = self.curr_last_pos_line_offset() + 1 + col;
        } else if self.cursor.line + 1 < self.lines.len() {
            // Go down one row to the next line if cursor is not already on the
            // last line.
            self.cursor.line += 1;
            if self.cursor.pos.row + 1 < self.window_height {
                self.cursor.pos.row += 1;
            }

            // Next line might be shorter than current cursor column position.
            let col = {
                let line = &self.lines[self.cursor.line];
                if line.is_empty() {
                    0
                } else if self.cursor.is_at_eol {
                    cmp::min(line.len(), self.window_width) - 1
                } else {
                    cmp::min(line.len() - 1, self.cursor.pos.col)
                }
            };

            log(format!("DOWN|new-line: col: {}", col).as_bytes());

            self.cursor.pos.col = col;
            self.cursor.byte = col;
        }
    }

    /// Shifts the window down by one row, but does not affect the cursor position.
    fn scroll_down(&mut self) {
        // Only scroll down if there's at least one line left, or if we're on
        // the last line but it's wrapped, so we can scroll to its next row.
        if self.cursor.line + 1 < self.lines.len() || self.curr_line_next_rows_len() > 0 {
            // The top row may be part of a wrapped line, so need to check if we
            // need to advance to the next line or just adjust the byte offset
            // from which to show the line.
            if self.line_offset_byte + self.window_width < self.lines[self.line_offset].len() {
                self.line_offset_byte += self.window_width;
                self.cursor.pos.row -= 1;
                log(format!("DOWN|scroll|wrap: line_offset: {}, line_offset_byte: {}, curr_line_next_rows_len: {}",
                            self.line_offset, self.line_offset_byte, self.curr_line_next_rows_len()).as_bytes());
            } else {
                self.line_offset += 1;
                self.line_offset_byte = 0;
                log(format!("DOWN|scroll|new-line: line_offset: {}, line_offset_byte: {}, self.cursor.line: {}",
                            self.line_offset, self.line_offset_byte, self.cursor.line).as_bytes());
                self.cursor.pos.row -= 1;
            }
        }
    }

    /// Moves the cursor up by one row, if possible.
    fn cursor_up(&mut self) {
        // Cursor may have reached the top of the window.
        if self.cursor.pos.row == 0 {
            self.scroll_up();
        }

        if self.cursor.byte >= self.window_width {
            // Line is wrapped so we don't have to skip to the previous line,
            // only the row.
            if self.cursor.pos.row > 0 {
                self.cursor.pos.row -= 1;
            }

            if self.cursor.is_at_eol {
                // Get the total length of the previous rows and subtract one to get the last
                // byte's offset in line of the previous row's last byte.
                self.cursor.byte = (self.cursor.byte / self.window_width) * self.window_width - 1;
                self.cursor.pos.col = self.cursor.byte % self.window_width;
            } else {
                self.cursor.byte -= self.window_width;
            }
        } else if self.cursor.line > 0 {
            // Cursor is on the first row of this line, so go to the previous
            // line.
            self.cursor.line -= 1;
            if self.cursor.pos.row > 0 {
                self.cursor.pos.row -= 1;
            }

            // Previous line might be shorter than current cursor column
            // position, in which case the cursor needs to be moved to its end,
            // and it might be wrapping, in which case the cursor needs to be
            // positioned on the last wrap of the line.
            let line = &self.lines[self.cursor.line];
            if line.is_empty() {
                self.cursor.pos.col = 0;
                self.cursor.byte = 0;
            } else {
                if line.len() <= self.window_width {
                    let col = {
                        if self.cursor.is_at_eol {
                            line.len() - 1
                        } else {
                            cmp::min(line.len() - 1, self.cursor.pos.col)
                        }
                    };

                    self.cursor.pos.col = col;
                    self.cursor.byte = col;
                } else {
                    // Use integer truncation to first get the number of full
                    // rows this line is broken up into.
                    let last_row_first_byte = (line.len() / self.window_width) * self.window_width;
                    let col = {
                        let last_row_len = line.len() - last_row_first_byte;
                        if self.cursor.is_at_eol {
                            last_row_len - 1
                        } else {
                            cmp::min(last_row_len - 1, self.cursor.pos.col)
                        }
                    };

                    self.cursor.byte = last_row_first_byte + col;
                    self.cursor.pos.col = col;
                }
            }
        }
    }

    /// Shifts the window up by one row, but does not affect the cursor position.
    fn scroll_up(&mut self) {
        // The top row may be part of a wrapped line, so need to check if we
        // need to advance to the previous line or just adjust the byte offset
        // from which to show the line.
        if self.line_offset_byte >= self.window_width {
            self.line_offset_byte -= self.window_width;
            //self.cursor.pos.row += 1;
        } else if self.line_offset > 0 {
            self.line_offset -= 1;
            //self.cursor.pos.row += 1;
            // If the previous line is wrapped, it must not be drawn from its first byte.
            let line = &self.lines[self.line_offset];
            if line.len() > self.window_width {
                self.line_offset_byte = (line.len() / self.window_width) * self.window_width;
            } else {
                self.line_offset_byte = 0;
            }
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor.pos.col > 0 {
            if self.cursor.pos.col == self.curr_last_pos_row_offset() {
                self.cursor.is_at_eol = false;
            }
            self.cursor.pos.col -= 1;
            self.cursor.byte -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor.byte + 1 < self.lines[self.cursor.line].len()
            && self.cursor.pos.col + 1 < self.window_width {
            self.cursor.pos.col += 1;
            self.cursor.byte += 1;
            if self.cursor.pos.col == self.curr_last_pos_row_offset() {
                self.cursor.is_at_eol = true;
            }
        }
    }

    /// Returns the position of the last byte in the row under the cursor.
    fn curr_last_pos_row_offset(&self) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let line = &self.lines[self.cursor.line];
        if line.is_empty() {
            0
        } else {
            assert!(self.window_width > 0);
            cmp::min(line.len(), self.window_width) - 1
        }
    }

    /// Similary to curr_last_pos_row_offset, but returns the that position's absolute
    /// offset from the line's start.
    fn curr_last_pos_line_offset(&self) -> usize {
        self.cursor.byte + self.curr_last_pos_row_offset() - self.cursor.pos.col
    }

    /// Returns the total number of bytes of all rows in this line after the row
    /// under the cursor.
    fn curr_line_next_rows_len(&self) -> usize {
        let line_len = self.lines[self.cursor.line].len();
        let row_last_byte = self.curr_last_pos_line_offset();
        if row_last_byte + 1 >= line_len { 0 } else { line_len - row_last_byte - 1 }
    }

    /// This function is called after encountering a \x1b escape character from
    /// stdin. It reads in the rest of the escape sequence and translates it to
    /// an optional Key value, or None, if no valid (or implemented) sequence
    /// was deteced.
    fn read_esc_seq_to_key(&mut self) -> Option<Key> {
        let mut buf: [u8; 3] = [0; 3];
        if let Err(_) = io::stdin().read_exact(&mut buf[..2]) {
            return None;
        }

        let c = buf[0] as char;
        if c == '[' {
            let c = buf[1] as char;
            if c >= '0' && c <= '9' {
                if let Err(_) = io::stdin().read_exact(&mut buf[2..3]) {
                    return None;
                }

                let c = buf[2] as char;
                return if c == '~' {
                    let c = buf[1] as char;
                    match c {
                        '1' | '7' => Some(Key::LineHome),
                        '4' | '8' => Some(Key::LineEnd),
                        '3' => Some(Key::Delete),
                        '5' => Some(Key::PageUp),
                        '6' => Some(Key::PageDown),
                        _ =>  None
                    }
                } else { None };
            } else {
                let c = buf[1] as char;
                match c {
                    'A' => Some(Key::ArrowUp),
                    'B' => Some(Key::ArrowDown),
                    'C' => Some(Key::ArrowRight),
                    'D' => Some(Key::ArrowLeft),
                    'H' => Some(Key::LineHome),
                    _ => None
                }
            }
        } else if c == 'O' {
            let c = buf[1] as char;
            match c {
                'H' => Some(Key::LineHome),
                'F' => Some(Key::LineEnd),
                _ => None
            }
        } else {
            None
        }
    }

    fn handle_input(&mut self, _c: char) {
    }

    fn refresh_screen(&mut self) {
        // Query window size as it may have been changed since the last redraw.
        // TODO if possible, listen to window resize events.
        self.update_window_size();
        // Hide cursor while redrawing to avoid glitching.
        self.hide_cursor();
        self.move_cursor(Pos { row: 0, col: 0 });
        // Append text to write buffer while clearing old data.
        self.build_rows();
        self.build_status_bar();
        self.update_status_msg();
        // (Rust giving me crap for directly passing self.cursor.pos.)
        let cursor = self.cursor.pos;
        // Move cursor back to its original position.
        self.move_cursor(cursor);
        self.show_cursor();
        self.defer_esc_seq("?25h");
        self.flush_write_buf();
    }

    fn line_orig_to_render(&self, line: &[u8]) -> Vec<u8> {
        let mut render = vec![];
        for (pos, b) in line.iter().enumerate() {
            if *b as char == '\t' {
                let mut i = pos + 1;
                render.push(' ' as u8);
                while i % self.config.tab_width as usize != 0 {
                    render.push(' ' as u8);
                    i += 1;
                }
            } else {
                render.push(*b);
            }
        }
        render
    }

    fn build_rows(&mut self) {
        let mut n_rows_drawn = 0;
        for line in self.lines.iter().skip(self.line_offset) {
            if n_rows_drawn == self.window_height {
                break;
            }

            // The line might be longer than the width of our window, so it needs
            // to be split accross rows and wrapped. Count how many bytes are left in
            // the row to draw.
            let (mut n_bytes_left, mut offset) = {
                if n_rows_drawn == 0 {
                    // This is the first line to draw which may not be drawn
                    // from its first byte if window begins after a wrap.
                    (line.len() - self.line_offset_byte, self.line_offset_byte)
                } else {
                    (line.len(), 0)
                }
            };

            // It's an empty line.
            if n_bytes_left == 0 {
                // Clear row.
                self.write_buf.extend("\x1b[K".as_bytes());
                n_rows_drawn += 1;
                if n_rows_drawn < self.window_height {
                    self.write_buf.extend("\r\n".as_bytes());
                } else {
                    self.write_buf.extend(" ".as_bytes());
                }
            } else {
                // Split up line into rows.
                while n_bytes_left > 0 && n_rows_drawn < self.window_height {
                    let end = offset + cmp::min(self.window_width, n_bytes_left);
                    let row = &line.render[offset..end];

                    assert!(row.len() > 0);
                    //log(format!("bytes left: {}, offset: {}, row.len: {}",
                            //n_bytes_left, offset, row.len()).as_bytes());

                    // Clear row.
                    // TODO we should use self.clear_row but can't due to ownership
                    self.write_buf.extend("\x1b[K".as_bytes());
                    self.write_buf.extend(row);
                    self.write_buf.extend("\r\n".as_bytes());

                    offset += row.len();
                    n_bytes_left -= row.len();
                    n_rows_drawn += 1;
                }
            }
        }

        log(format!("window height: {}, rows drawn: {}",
                    self.window_height, n_rows_drawn).as_bytes());
        // There may not be enough text to fill all the rows of the window, so
        // fill the rest with '~'s.
        let n_empty_rows = self.window_height - n_rows_drawn;
        if n_empty_rows > 0 {
            for _ in 1..(n_empty_rows) {
                self.write_buf.extend("~\r\n".as_bytes());
                self.clear_row();
            }
        }
    }

    fn build_status_bar(&mut self) {
        // TODO also count escape sequences
        self.write_buf.reserve(self.window_width);

        // Invert colors.
        self.defer_esc_seq("1m");
        // Make text bold.
        self.defer_esc_seq("7m");

        let sep = " | ";
        let line_count = {
            let mut buf = self.lines.len().to_string();
            if self.lines.len() == 1 {
                buf += " line";
            } else {
                buf += " lines";
            }
            buf
        };
        let cursor_pos = {
            let mut buf = self.cursor.line.to_string();
            buf += ":";
            buf += &self.cursor.pos.col.to_string()[..];
            buf
        };
        let (n_used_bytes, n_path_bytes) = {
            // NOTE: count separators as well: one separator between path and
            // cursor position, and one between the latter and line count.
            let mut n_used_bytes = cursor_pos.len() + line_count.len() + sep.len() * 1;
            let n_path_bytes = cmp::min(self.window_width - n_used_bytes, self.path.len());
            n_used_bytes += n_path_bytes;
            (n_used_bytes, n_path_bytes)
        };

        self.write_buf.extend(self.path.as_bytes().iter().take(n_path_bytes));
        // Fill up empty space.
        //self.write_buf.extend(std::iter::repeat(' ' as u8).take(self.window_width - n_used_bytes));
        for _ in 0..self.window_width - n_used_bytes {
            self.write_buf.push(' ' as u8);
        }
        self.write_buf.extend(cursor_pos.as_bytes().iter());
        self.write_buf.extend(sep.as_bytes().iter());
        self.write_buf.extend(line_count.as_bytes().iter());

        log(format!("status bar buffer: {:?}", &self.write_buf[self.write_buf.len() - self.window_width..]).as_bytes());
        // Revert invert colors.
        self.defer_esc_seq("m");
    }

    fn new_status_msg(&mut self, msg: &str, timeout: Duration) {
        //let len = cmp::min(self.window_width, msg.len());
        //self.write_buf.extend(msg.as_bytes().iter().take(len));
        self.status_msg = StatusMsg {
            data: msg.to_string(),
            timestamp: Instant::now(),
            timeout: timeout,
        };
        self.write_status_msg();
    }

    fn update_status_msg(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.status_msg.timestamp) <= self.status_msg.timeout {
            self.write_status_msg();
        } else {
            self.status_msg.data.clear();
        }
    }

    fn write_status_msg(&mut self) {
        let len = cmp::min(self.window_width, self.status_msg.data.len());
        self.write_buf.extend(self.status_msg.data.as_bytes().iter().take(len));
    }

    fn flush_write_buf(&mut self) {
        io::stdout().write(&self.write_buf).unwrap();
        io::stdout().flush().unwrap();
        // Does not alter its capacity.
        self.write_buf.clear();
    }

    fn move_cursor(&mut self, pos: Pos) {
        self.defer_esc_seq(&format!("{};{}H", pos.row + 1, pos.col + 1));
    }

    fn hide_cursor(&mut self) {
        self.defer_esc_seq("?25l");
    }

    fn show_cursor(&mut self) {
        self.defer_esc_seq("?25h");
    }

    fn clear_screen(&mut self) {
        self.defer_esc_seq("2J");
    }

    fn clear_row(&mut self) {
        self.defer_esc_seq("K");
    }

    /// Appends the specified escape sequence to the write buffer which needs to
    /// be manually flushed for the sequence to take effect.
    fn defer_esc_seq(&mut self, cmd: &str) {
        self.write_buf.extend(format!("\x1b[{}", cmd).as_bytes());
    }

    /// Immeadiately sends the specified escape sequence to the terminal.
    fn send_esc_seq(&mut self, cmd: &str) {
        println!("\x1b[{}", cmd);
    }

    fn update_window_size(&mut self) {
        // Move cursor as far right and down as we can (set_cursor_pos not used
        // on purpose as it uses a different escape sequence which does not
        // ensure that it won't move the cursor beyond the confines of the
        // window while this does).
        self.send_esc_seq("999C");
        self.send_esc_seq("999B");
        let bottom_right_corner = self.cursor_pos();
        self.window_width = bottom_right_corner.col + 1;
        // NOTE: subtract 2 from the result: 1 for the status bar and 1 for the
        // status message bar (only subtract one since the + 1 hasn't been added
        // to begin with).
        self.window_height = bottom_right_corner.row - 1;
    }

    fn cursor_pos(&mut self) -> Pos {
        // Query cursor position.
        self.send_esc_seq("6n");

        // Read response from stdin. The response should look like this:
        // \x1b[<number>;<number>
        // So if we generously assume each number to be 3 digits long, 10
        // bytes should be enough to allocate only once.
        let mut response = String::with_capacity(10);
        for r in io::stdin().bytes() {
            match r {
                Ok(c) => {
                    if c == 'R' as u8 {
                        break;
                    } else {
                        response.push(c as char);
                    }
                }
                Err(_) => (),
            }
        }

        // Sometimes we receive a [6~ (which as far as I can tell is not a
        // valid escape sequence), so skip to the first \x1b character.
        let esc_pos = response.find('\x1b').unwrap();
        let response = &response[esc_pos + 1..];
        let row_pos = response.find(char::is_numeric).unwrap();
        let semicolon_pos = response.find(';').unwrap();
        assert!(row_pos < semicolon_pos);
        let row: usize = response[row_pos..semicolon_pos].parse().unwrap();

        // Skip the first integer.
        assert!(semicolon_pos < response.len());
        let response = &response[semicolon_pos..];

        let col_pos = response.find(char::is_numeric).unwrap();
        assert!(col_pos < response.len());
        let col: usize = response[col_pos..].parse().unwrap();

        Pos { col: col - 1, row: row - 1 }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Restore user's screen.
        self.clear_screen();
    }
}

fn init_log() {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("/tmp/kilo-rust.log")
        .unwrap();
}

fn log(buf: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .append(true)
        .open("/tmp/kilo-rust.log")
        .unwrap();
    file.write("\n>>NEW LOG ENTRY\n".as_bytes()).unwrap();
    file.write(&buf).unwrap();
    file.write("\n".as_bytes()).unwrap();
    file.flush().unwrap();
}

fn main() {
    init_log();
    // Save the current terminal config before entering raw mode with the
    // instantiation of the editor so that we can restore it on drop.
    let orig_termios = termios::tcgetattr(io::stdin().as_raw_fd()).unwrap();
    let mut raw_termios = orig_termios.clone();

    termios::cfmakeraw(&mut raw_termios);
    termios::tcsetattr(
        io::stdin().as_raw_fd(),
        termios::SetArg::TCSANOW,
        &raw_termios,
    ).unwrap();

    let config = Config { tab_width: 4 };

    let args: Vec<String> = args().collect();
    if args.len() > 1 {
        Editor::open_file(config, Path::new(&args[1])).unwrap().run();
    } else {
        // TODO report error or ask for a file name
    }

    // Restore the original termios config.
    termios::tcsetattr(
        io::stdin().as_raw_fd(),
        termios::SetArg::TCSANOW,
        &orig_termios,
    ).unwrap();
}
