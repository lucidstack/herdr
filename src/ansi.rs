//! Removes terminal control sequences from text a process printed, keeping what a person
//! would read. Shared by every feature that shows captured output outside a terminal.

#[derive(Clone, Copy)]
enum ControlSequenceState {
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    StString,
}

pub(crate) fn strip_terminal_control_sequences(value: &[u8]) -> Vec<u8> {
    use ControlSequenceState::*;

    let mut output = Vec::with_capacity(value.len());
    let mut state = Text;
    for &byte in value {
        state = match (state, byte) {
            (Text, b'\x1b') => Escape,
            (Text, _) => {
                output.push(byte);
                Text
            }
            (Escape, b'[') => Csi,
            (Escape, b']') => Osc,
            (Escape, b'P' | b'X' | b'^' | b'_') => StString,
            (Escape, 0x20..=0x2f) => EscapeIntermediate,
            (Escape, 0x30..=0x7e) => Text,
            (Escape, b'\x1b') => Escape,
            (Escape, b'\x18' | b'\x1a') => Text,
            (Escape, byte) if byte.is_ascii_control() => Escape,
            (Escape, _) => {
                output.push(byte);
                Text
            }
            (EscapeIntermediate, 0x20..=0x2f) => EscapeIntermediate,
            (EscapeIntermediate, 0x30..=0x7e) => Text,
            (EscapeIntermediate, b'\x1b') => Escape,
            (EscapeIntermediate, b'\x18' | b'\x1a') => Text,
            (EscapeIntermediate, byte) if byte.is_ascii_control() => EscapeIntermediate,
            (EscapeIntermediate, _) => {
                output.push(byte);
                Text
            }
            (Csi, 0x20..=0x3f) => Csi,
            (Csi, 0x40..=0x7e) => Text,
            (Csi, b'\x1b') => Escape,
            (Csi, b'\x18' | b'\x1a') => Text,
            (Csi, byte) if byte.is_ascii_control() => Csi,
            (Csi, _) => {
                output.push(byte);
                Text
            }
            (Osc, b'\x07') => Text,
            (Osc, b'\x1b') => Escape,
            (Osc, b'\x18' | b'\x1a') => Text,
            (Osc, _) => Osc,
            (StString, b'\x1b') => Escape,
            (StString, b'\x18' | b'\x1a') => Text,
            (StString, _) => StString,
        };
    }
    output
}
