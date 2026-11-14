//! Keyboard layouts: map PS/2 scancodes (set 1, by physical key position) to
//! characters. Each row lists the characters of consecutive scancodes for the
//! normal, Shift and AltGr levels; '\0' means "nothing".

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// English (US) QWERTY.
    Us,
    /// Belgian AZERTY, keypad decimal key types '.' ("Belgian (Period)").
    BelgianPeriod,
}

/// What a key produces on a given layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapped {
    Char(char),
    /// A dead key: combines with the next character (e.g. ^ + e = ê).
    Dead(char),
}

struct Row {
    first: u8,
    normal: &'static str,
    shift: &'static str,
    altgr: &'static str,
}

const fn row(first: u8, normal: &'static str, shift: &'static str, altgr: &'static str) -> Row {
    Row { first, normal, shift, altgr }
}

const US: &[Row] = &[
    row(0x02, "1234567890-=", "!@#$%^&*()_+", ""),
    row(0x10, "qwertyuiop[]", "QWERTYUIOP{}", ""),
    row(0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~", ""),
    row(0x2B, "\\zxcvbnm,./", "|ZXCVBNM<>?", ""),
    row(0x39, " ", " ", " "),
    row(0x56, "\\", "|", ""), // extra key on ISO keyboards
];

const BE: &[Row] = &[
    row(0x02, "&é\"'(§è!çà)-", "1234567890°_", "|@#\0\0^\0\0{}\0\0"),
    row(0x10, "azertyuiop\0$", "AZERTYUIOP\0*", "\0\0€\0\0\0\0\0\0\0[]"),
    row(0x1E, "qsdfghjklmù²", "QSDFGHJKLM%³", ""),
    row(0x2B, "µwxcvbn,;:=", "£WXCVBN?./+", ""),
    row(0x39, " ", " ", " "),
    row(0x56, "<", ">", "\\"),
];

/// Belgian dead keys: (scancode, level 0=normal 1=shift 2=altgr, accent).
const BE_DEAD: &[(u8, u8, char)] = &[
    (0x1A, 0, '^'),
    (0x1A, 1, '¨'),
    (0x28, 2, '´'),
    (0x2B, 2, '`'),
    (0x35, 2, '~'),
];

impl Layout {
    pub const ALL: [Layout; 2] = [Layout::Us, Layout::BelgianPeriod];

    /// Short identifier used by the `layout` command and the config file.
    pub fn id(self) -> &'static str {
        match self {
            Layout::Us => "us",
            Layout::BelgianPeriod => "be",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Layout::Us => "English (US)",
            Layout::BelgianPeriod => "Belgian (Period)",
        }
    }

    pub fn from_id(id: &str) -> Option<Layout> {
        Layout::ALL.into_iter().find(|l| l.id() == id)
    }

    /// Character typed by the numeric keypad's decimal key (Num Lock on).
    pub fn keypad_decimal(self) -> char {
        match self {
            Layout::Us | Layout::BelgianPeriod => '.',
        }
    }

    /// Map a scancode at a modifier level. Caps Lock is applied by the caller.
    pub fn map(self, scancode: u8, shift: bool, altgr: bool) -> Option<Mapped> {
        let level = if altgr { 2 } else if shift { 1 } else { 0 };
        let (rows, dead) = match self {
            Layout::Us => (US, &[][..]),
            Layout::BelgianPeriod => (BE, BE_DEAD),
        };
        if let Some(&(_, _, accent)) = dead.iter().find(|&&(sc, lv, _)| sc == scancode && lv == level) {
            return Some(Mapped::Dead(accent));
        }
        let row = rows.iter().find(|r| scancode >= r.first && ((scancode - r.first) as usize) < r.normal.chars().count())?;
        let chars = match level {
            0 => row.normal,
            1 => row.shift,
            _ => row.altgr,
        };
        match chars.chars().nth((scancode - row.first) as usize) {
            Some('\0') | None => None,
            Some(c) => Some(Mapped::Char(c)),
        }
    }
}

/// Combine a dead-key accent with a character (e.g. '^' + 'e' = 'ê').
pub fn compose(accent: char, c: char) -> Option<char> {
    let (base, out): (&str, &str) = match accent {
        '^' => ("aeiouAEIOU", "âêîôûÂÊÎÔÛ"),
        '¨' => ("aeiouyAEIOU", "äëïöüÿÄËÏÖÜ"),
        '´' => ("aeiouyAEIOUY", "áéíóúýÁÉÍÓÚÝ"),
        '`' => ("aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
        '~' => ("aonAON", "ãõñÃÕÑ"),
        _ => return None,
    };
    base.chars().position(|b| b == c).and_then(|i| out.chars().nth(i))
}
