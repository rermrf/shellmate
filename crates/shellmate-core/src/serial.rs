//! Persisted serial framing and session send options; independent of any OS driver.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Newline {
    #[default]
    CrLf,
    Lf,
    Cr,
}
impl Newline {
    pub fn label(self) -> &'static str {
        match self {
            Self::CrLf => "CRLF",
            Self::Lf => "LF",
            Self::Cr => "CR",
        }
    }
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::CrLf => b"\r\n",
            Self::Lf => b"\n",
            Self::Cr => b"\r",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::CrLf => Self::Lf,
            Self::Lf => Self::Cr,
            Self::Cr => Self::CrLf,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialSettings {
    pub baud: u32,
    pub data_bits: u8,
    /// N (none), E (even), O (odd).
    pub parity: char,
    pub stop_bits: u8,
    /// none, software, hardware.
    pub flow: String,
    pub newline: Newline,
}
impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 115200,
            data_bits: 8,
            parity: 'N',
            stop_bits: 1,
            flow: "none".into(),
            newline: Newline::CrLf,
        }
    }
}
impl SerialSettings {
    pub fn framing(&self) -> String {
        format!("{}{}{}", self.data_bits, self.parity, self.stop_bits)
    }
    pub fn label(&self) -> String {
        format!("{} · {}", self.baud, self.framing())
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.baud == 0 {
            return Err("波特率必须大于 0");
        }
        if !(5..=8).contains(&self.data_bits) {
            return Err("数据位必须为 5–8");
        }
        if !['N', 'E', 'O'].contains(&self.parity) {
            return Err("校验必须为无、偶或奇校验");
        }
        if ![1, 2].contains(&self.stop_bits) {
            return Err("停止位必须为 1 或 2");
        }
        if !["none", "software", "hardware"].contains(&self.flow.as_str()) {
            return Err("流控选项无效");
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_and_validation() {
        let mut s = SerialSettings::default();
        assert_eq!(s.label(), "115200 · 8N1");
        assert_eq!(s.validate(), Ok(()));
        s.baud = 0;
        assert!(s.validate().is_err());
        s.baud = 9600;
        s.data_bits = 9;
        assert!(s.validate().is_err());
        assert_eq!(Newline::CrLf.next().bytes(), b"\n");
        assert_eq!(Newline::CrLf.next().next().next(), Newline::CrLf);
    }
}
