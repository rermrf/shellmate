//! Incremental RFC 854 NVT decoding. Options are server-initiated, so there are
//! no outstanding requests to cross. Remember acknowledgements/refusals until
//! the peer withdraws them; repeated announcements never produce a loop.
use shellmate_core::PtyDims;
pub const IAC: u8 = 255;
pub const WILL: u8 = 251;
pub const WONT: u8 = 252;
pub const DO: u8 = 253;
pub const DONT: u8 = 254;
const SB: u8 = 250;
const SE: u8 = 240;
const ECHO: u8 = 1;
const SGA: u8 = 3;
const TTYPE: u8 = 24;
const NAWS: u8 = 31;
#[derive(Default)]
enum State {
    #[default]
    Data,
    Iac,
    Option(u8),
    Sub,
    SubIac,
}
pub struct Parser {
    state: State,
    sub: Vec<u8>,
    remote: [bool; 256],
    local: [bool; 256],
    refused_remote: [bool; 256],
    refused_local: [bool; 256],
    cr: bool,
    dims: PtyDims,
}
impl Parser {
    pub fn new(dims: PtyDims) -> Self {
        Self {
            state: State::Data,
            sub: Vec::new(),
            remote: [false; 256],
            local: [false; 256],
            refused_remote: [false; 256],
            refused_local: [false; 256],
            cr: false,
            dims,
        }
    }
    /// Returns display bytes and protocol replies separately.
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (mut out, mut reply) = (Vec::new(), Vec::new());
        for &b in bytes {
            match std::mem::take(&mut self.state) {
                State::Data if b == IAC => self.state = State::Iac,
                State::Data => self.data(b, &mut out),
                State::Iac => match b {
                    IAC => self.data(b, &mut out),
                    WILL | WONT | DO | DONT => self.state = State::Option(b),
                    SB => {
                        self.sub.clear();
                        self.state = State::Sub;
                    }
                    _ => {}
                },
                State::Option(cmd) => self.option(cmd, b, &mut reply),
                State::Sub => {
                    if b == IAC {
                        self.state = State::SubIac;
                    } else {
                        if self.sub.len() < 1024 {
                            self.sub.push(b);
                        }
                        self.state = State::Sub;
                    }
                }
                State::SubIac => {
                    if b == SE {
                        if self.local[TTYPE as usize] && self.sub == [TTYPE, 1] {
                            reply.extend([IAC, SB, TTYPE, 0]);
                            reply.extend(b"xterm-256color");
                            reply.extend([IAC, SE]);
                        }
                    } else {
                        if b == IAC && self.sub.len() < 1024 {
                            self.sub.push(IAC);
                        }
                        self.state = State::Sub;
                    }
                }
            }
        }
        (out, reply)
    }
    fn data(&mut self, b: u8, out: &mut Vec<u8>) {
        // Deliver CR immediately (interactive cursor motion), swallow only its NUL padding.
        if self.cr && b == 0 {
            self.cr = false;
            return;
        }
        self.cr = b == b'\r';
        out.push(b);
    }
    fn option(&mut self, cmd: u8, opt: u8, out: &mut Vec<u8>) {
        let i = opt as usize;
        match cmd {
            WILL if matches!(opt, ECHO | SGA) => {
                if !self.remote[i] {
                    self.remote[i] = true;
                    out.extend([IAC, DO, opt]);
                }
            }
            WILL => {
                if !self.refused_remote[i] {
                    self.refused_remote[i] = true;
                    out.extend([IAC, DONT, opt]);
                }
            }
            WONT => {
                if self.remote[i] {
                    self.remote[i] = false;
                    out.extend([IAC, DONT, opt]);
                }
                self.refused_remote[i] = false;
            }
            DO if matches!(opt, SGA | TTYPE | NAWS) => {
                if !self.local[i] {
                    self.local[i] = true;
                    out.extend([IAC, WILL, opt]);
                    if opt == NAWS {
                        out.extend(self.naws());
                    }
                }
            }
            DO => {
                if !self.refused_local[i] {
                    self.refused_local[i] = true;
                    out.extend([IAC, WONT, opt]);
                }
            }
            DONT => {
                if self.local[i] {
                    self.local[i] = false;
                    out.extend([IAC, WONT, opt]);
                }
                self.refused_local[i] = false;
            }
            _ => {}
        }
    }
    fn naws(&self) -> Vec<u8> {
        let mut out = vec![IAC, SB, NAWS];
        for b in self
            .dims
            .cols
            .to_be_bytes()
            .into_iter()
            .chain(self.dims.rows.to_be_bytes())
        {
            out.push(b);
            if b == IAC {
                out.push(b);
            }
        }
        out.extend([IAC, SE]);
        out
    }
    pub fn resize(&mut self, cols: u16, rows: u16) -> Vec<u8> {
        self.dims.cols = cols;
        self.dims.rows = rows;
        if self.local[NAWS as usize] {
            self.naws()
        } else {
            Vec::new()
        }
    }
}
/// NVT output: CR not followed by LF is padded with NUL; LF alone is CR LF.
/// Every literal IAC is doubled, including non-UTF8 input.
pub fn encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            IAC => out.extend([IAC, IAC]),
            b'\r' => {
                out.push(b'\r');
                if bytes.get(i + 1) == Some(&b'\n') {
                    out.push(b'\n');
                    i += 1;
                } else {
                    out.push(0);
                }
            }
            b'\n' => out.extend(b"\r\n"),
            b => out.push(b),
        }
        i += 1;
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_negotiation_and_no_loops() {
        let mut p = Parser::new(PtyDims::default());
        let input = [
            IAC, WILL, ECHO, IAC, WILL, SGA, IAC, DO, TTYPE, IAC, DO, NAWS, IAC, WILL, 42, IAC, DO,
            42,
        ];
        assert_eq!(
            p.feed(&input).1,
            [
                IAC, DO, ECHO, IAC, DO, SGA, IAC, WILL, TTYPE, IAC, WILL, NAWS, IAC, SB, NAWS, 0,
                80, 0, 24, IAC, SE, IAC, DONT, 42, IAC, WONT, 42
            ]
        );
        assert!(p.feed(&input).1.is_empty());
        assert_eq!(
            p.feed(&[IAC, SB, TTYPE, 1, IAC, SE]).1,
            [&[IAC, SB, TTYPE, 0][..], b"xterm-256color", &[IAC, SE]].concat()
        );
        assert_eq!(
            p.resize(255, 50),
            [IAC, SB, NAWS, 0, 255, 255, 0, 50, IAC, SE]
        );
        assert_eq!(p.feed(&[IAC, DONT, NAWS]).1, [IAC, WONT, NAWS]);
        assert!(p.resize(80, 24).is_empty());
    }
    #[test]
    fn inetutils_25_opening_transcript_answers_every_option() {
        let opening = [
            IAC, WILL, 37, IAC, WILL, 38, IAC, DO, 24, IAC, DO, 32, IAC, DO, 35, IAC, DO, 39, IAC,
            DO, 36,
        ];
        let expected = [
            IAC, DONT, 37, IAC, DONT, 38, IAC, WILL, 24, IAC, WONT, 32, IAC, WONT, 35, IAC, WONT,
            39, IAC, WONT, 36,
        ];
        for chunk_size in 1..=opening.len() {
            let mut p = Parser::new(PtyDims::default());
            let mut replies = Vec::new();
            for chunk in opening.chunks(chunk_size) {
                let (out, reply) = p.feed(chunk);
                assert!(out.is_empty());
                replies.extend(reply);
            }
            assert_eq!(replies, expected);
            assert!(
                p.feed(&opening).1.is_empty(),
                "duplicate requests must not loop"
            );
            assert_eq!(
                p.feed(&[IAC, SB, TTYPE, 1, IAC, SE]).1,
                [&[IAC, SB, TTYPE, 0][..], b"xterm-256color", &[IAC, SE]].concat()
            );
        }
    }

    #[test]
    fn chunked_nvt_and_iac() {
        let wire = b"a\r\0b\r\nc\xff\xffd";
        let mut p = Parser::new(PtyDims::default());
        let mut out = Vec::new();
        for b in wire {
            out.extend(p.feed(&[*b]).0);
        }
        assert_eq!(out, b"a\rb\r\nc\xffd");
        assert_eq!(encode(b"a\r\nb\rc\n\xff"), b"a\r\nb\r\0c\r\n\xff\xff");
    }
}
