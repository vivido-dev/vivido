//! Control data of one Kitty graphics command.
//!
//! A graphics command is `ESC _ G <control> ; <payload> ESC \`, where the control data is a
//! comma-separated list of `key=value` pairs. This module turns that list into a [`Command`]; it
//! knows nothing about what the keys mean.

// Rust guideline compliant 2026-10-07

/// Most control bytes one command may carry.
///
/// The protocol defines about thirty keys whose values are at most eleven characters, so a
/// conforming command stays well under 400 bytes. Anything longer is not a graphics command worth
/// buffering and is dropped.
pub(crate) const MAX_CONTROL_BYTES: usize = 512;

/// Parsed control data, with the protocol's zero defaults for absent keys.
///
/// Field names follow the protocol's own vocabulary; the key letter is given for each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Command {
    /// `a`: what to do. Zero when absent, which the protocol treats as `t`.
    pub action: u8,
    /// `d`: which placements a delete addresses. Zero when absent, treated as `a`.
    pub delete: u8,
    /// `t`: how the payload is delivered. Zero when absent, treated as `d` (direct).
    pub medium: u8,
    /// `o`: payload compression, `z` or zero for none.
    pub compression: u8,
    /// `f`: pixel format (24, 32, or 100 for PNG). Zero when absent, treated as 32.
    pub format: u32,
    /// `m`: nonzero when more chunks of this transmission follow.
    pub more: u32,
    /// `i`: image id.
    pub id: u32,
    /// `I`: image number.
    pub number: u32,
    /// `p`: placement id.
    pub placement: u32,
    /// `q`: reply suppression (1 hides success, 2 hides everything).
    pub quiet: u32,
    /// `w`: width of the source rectangle, zero for the rest of the image.
    pub width: u32,
    /// `h`: height of the source rectangle, zero for the rest of the image.
    pub height: u32,
    /// `x`: left edge of the source rectangle; a column or range start for deletes.
    pub x: u32,
    /// `y`: top edge of the source rectangle; a row or range end for deletes.
    pub y: u32,
    /// `s`: width of the transmitted image in pixels.
    pub data_width: u32,
    /// `v`: height of the transmitted image in pixels.
    pub data_height: u32,
    /// `S`: size of the data to read from a file. Accepted and unused.
    pub data_size: u32,
    /// `O`: offset of the data in a file. Accepted and unused.
    pub data_offset: u32,
    /// `c`: columns the placement should span, zero to derive it.
    pub columns: u32,
    /// `r`: rows the placement should span, zero to derive it.
    pub rows: u32,
    /// `X`: horizontal pixel offset inside the first cell.
    pub cell_x: u32,
    /// `Y`: vertical pixel offset inside the first cell.
    pub cell_y: u32,
    /// `z`: stacking order.
    pub z: i32,
    /// `C`: 1 leaves the cursor where it was after a placement.
    pub cursor: u32,
    /// `U`: 1 creates a virtual placement for Unicode placeholders.
    pub unicode: u32,
    /// `P`: parent image id of a relative placement.
    pub parent: u32,
    /// `Q`: parent placement id of a relative placement.
    pub parent_placement: u32,
    /// `H`: horizontal cell offset from the parent.
    pub parent_x: i32,
    /// `V`: vertical cell offset from the parent.
    pub parent_y: i32,
    /// `N`: usage hints. Accepted and unused.
    pub hints: u32,
}

/// Parse the control data of one command.
///
/// Returns `None` for anything malformed: an unknown key, a missing `=`, a value that is not a
/// number of the key's type, or a flag outside the key's alphabet. Kitty drops such a command
/// without a reply, and so does the caller.
pub(crate) fn parse(control: &[u8]) -> Option<Command> {
    let mut command = Command::default();
    if control.is_empty() {
        return Some(command);
    }
    for pair in control.split(|&byte| byte == b',') {
        let (&key, rest) = pair.split_first()?;
        let (&equals, value) = rest.split_first()?;
        if equals != b'=' {
            return None;
        }
        match key {
            b'a' => command.action = flag(value, b"Tacdfpqt")?,
            b'd' => command.delete = flag(value, b"AaCcFfIiNnPpQqRrXxYyZz")?,
            b't' => command.medium = flag(value, b"dfst")?,
            b'o' => command.compression = flag(value, b"z")?,
            b'f' => command.format = unsigned(value)?,
            b'm' => command.more = unsigned(value)?,
            b'i' => command.id = unsigned(value)?,
            b'I' => command.number = unsigned(value)?,
            b'p' => command.placement = unsigned(value)?,
            b'q' => command.quiet = unsigned(value)?,
            b'w' => command.width = unsigned(value)?,
            b'h' => command.height = unsigned(value)?,
            b'x' => command.x = unsigned(value)?,
            b'y' => command.y = unsigned(value)?,
            b's' => command.data_width = unsigned(value)?,
            b'v' => command.data_height = unsigned(value)?,
            b'S' => command.data_size = unsigned(value)?,
            b'O' => command.data_offset = unsigned(value)?,
            b'c' => command.columns = unsigned(value)?,
            b'r' => command.rows = unsigned(value)?,
            b'X' => command.cell_x = unsigned(value)?,
            b'Y' => command.cell_y = unsigned(value)?,
            b'z' => command.z = signed(value)?,
            b'C' => command.cursor = unsigned(value)?,
            b'U' => command.unicode = unsigned(value)?,
            b'P' => command.parent = unsigned(value)?,
            b'Q' => command.parent_placement = unsigned(value)?,
            b'H' => command.parent_x = signed(value)?,
            b'V' => command.parent_y = signed(value)?,
            b'N' => command.hints = unsigned(value)?,
            _ => return None,
        }
    }
    Some(command)
}

/// A single-character value that must be one of `allowed`.
fn flag(value: &[u8], allowed: &[u8]) -> Option<u8> {
    match value {
        [flag] if allowed.contains(flag) => Some(*flag),
        _ => None,
    }
}

/// A decimal `u32` with no sign, no padding and no overflow.
fn unsigned(value: &[u8]) -> Option<u32> {
    if value.is_empty() {
        return None;
    }
    value.iter().try_fold(0_u32, |total, byte| {
        let digit = u32::from(byte.checked_sub(b'0').filter(|digit| *digit <= 9)?);
        total.checked_mul(10)?.checked_add(digit)
    })
}

/// A decimal `i32` with an optional leading minus.
fn signed(value: &[u8]) -> Option<i32> {
    match value.split_first() {
        Some((b'-', digits)) => 0_i32.checked_sub_unsigned(unsigned(digits)?),
        _ => i32::try_from(unsigned(value)?).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transmission_kitim_sends_is_parsed_field_by_field() {
        let command = parse(b"a=T,f=32,s=640,v=360,C=1,q=2,m=1").unwrap();
        assert_eq!(
            command,
            Command {
                action: b'T',
                format: 32,
                data_width: 640,
                data_height: 360,
                cursor: 1,
                quiet: 2,
                more: 1,
                ..Command::default()
            }
        );
    }

    #[test]
    fn every_key_reaches_its_own_field() {
        let command = parse(
            b"a=p,d=Z,t=d,o=z,f=100,m=0,i=1,I=2,p=3,q=1,w=4,h=5,x=6,y=7,s=8,v=9,S=10,O=11,c=12,\
              r=13,X=14,Y=15,z=-16,C=1,U=1,P=17,Q=18,H=-19,V=20,N=21",
        )
        .unwrap();
        assert_eq!(
            command,
            Command {
                action: b'p',
                delete: b'Z',
                medium: b'd',
                compression: b'z',
                format: 100,
                more: 0,
                id: 1,
                number: 2,
                placement: 3,
                quiet: 1,
                width: 4,
                height: 5,
                x: 6,
                y: 7,
                data_width: 8,
                data_height: 9,
                data_size: 10,
                data_offset: 11,
                columns: 12,
                rows: 13,
                cell_x: 14,
                cell_y: 15,
                z: -16,
                cursor: 1,
                unicode: 1,
                parent: 17,
                parent_placement: 18,
                parent_x: -19,
                parent_y: 20,
                hints: 21,
            }
        );
    }

    #[test]
    fn empty_control_data_is_a_command_of_defaults() {
        assert_eq!(parse(b""), Some(Command::default()));
    }

    #[test]
    fn numbers_cover_their_whole_range_and_nothing_beyond_it() {
        assert_eq!(parse(b"i=4294967295").unwrap().id, u32::MAX);
        assert_eq!(parse(b"i=4294967296"), None);
        assert_eq!(parse(b"z=-2147483648").unwrap().z, i32::MIN);
        assert_eq!(parse(b"z=2147483647").unwrap().z, i32::MAX);
        assert_eq!(parse(b"z=2147483648"), None);
        assert_eq!(parse(b"z=-2147483649"), None);
    }

    #[test]
    fn malformed_control_data_is_rejected_whole() {
        for control in [
            &b"a"[..],
            b"a=",
            b"a=x",
            b"a=TT",
            b"i",
            b"i=",
            b"i=-1",
            b"i=1x",
            b"i=+1",
            b"i= 1",
            b"z=-",
            b"z=--1",
            b"k=1",
            b"i:1",
            b"i=1,",
            b",i=1",
            b"i=1,,s=2",
            b"t=x",
            b"o=y",
            b"d=b",
        ] {
            assert_eq!(parse(control), None, "{:?}", std::str::from_utf8(control));
        }
    }

    #[test]
    fn a_repeated_key_keeps_its_last_value() {
        assert_eq!(parse(b"i=1,i=2").unwrap().id, 2);
    }
}
