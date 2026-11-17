//! TWS wire framing and field codec.
//!
//! The TWS API socket is a byte stream of length-prefixed messages. Each message
//! is a big-endian `u32` body length followed by that many bytes of NUL-terminated
//! UTF-8 fields. The last field is NUL-terminated too, so the body always ends
//! with a `0x00` byte; an intentionally empty last field therefore leaves two
//! trailing NULs. This mirrors `trama()` in the upstream `trading-engine.test.ts`
//! oracle exactly.

use thiserror::Error;

/// Largest body accepted from the broker (16 MiB). Anything beyond this is a
/// protocol error, not a slow stream: a corrupt length prefix would otherwise
/// make the decoder wait for an impossible amount of data.
pub const MAX_BODY_LEN: usize = 16 * 1024 * 1024;

/// A protocol violation detected while decoding a TWS frame.
///
/// `next_message()` keeps the `Option<Vec<String>>` shape of a streaming decoder:
/// it never returns a `Result`. Errors are parked inside the decoder and can be
/// taken with [`Decoder::take_error`] so the client can surface them as events.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    /// The peer announced a body larger than [`MAX_BODY_LEN`].
    #[error("La trama TWS anuncia {length} bytes, por encima del límite de {limit} bytes")]
    FrameTooLarge { length: usize, limit: usize },
    /// A field was not valid UTF-8. The message is still delivered, with the
    /// invalid sequences replaced by U+FFFD, because broker data must never
    /// bring down the client. See the note on [`Decoder::next_message`].
    #[error("La trama TWS contiene UTF-8 inválido; los campos se decodificaron de forma tolerante")]
    InvalidUtf8,
}

/// Encodes TWS fields into one complete wire frame.
///
/// Every field is appended verbatim followed by a NUL byte, including an empty
/// field; the resulting body is prefixed with its big-endian `u32` byte length.
/// TWS itself caps messages well below `u32::MAX`, so the cast cannot truncate
/// a frame produced by this client in practice.
pub fn encode_message(fields: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    for field in fields {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }

    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Incremental frame decoder over a byte buffer.
///
/// [`Decoder::push`] appends whatever the socket produced; [`Decoder::next_message`]
/// yields complete messages and leaves partial ones buffered until the rest
/// arrives. Several messages in one push are returned one at a time.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    error: Option<DecodeError>,
}

impl Decoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends freshly read bytes to the internal buffer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Returns the next complete message, or `None` when more bytes are needed.
    ///
    /// Fields are decoded with `String::from_utf8_lossy`: a symbol with a damaged
    /// byte is a data problem, not a reason to panic or lose the whole message.
    /// When that happens an [`DecodeError::InvalidUtf8`] is also parked for
    /// reporting via [`Decoder::take_error`], so the caller can still raise a
    /// protocol warning. An oversized length is fatal for the current buffer:
    /// the bytes are discarded (there is no safe resynchronisation point) and
    /// a [`DecodeError::FrameTooLarge`] is parked. Empty fields are preserved:
    /// exactly one trailing NUL — the field terminator — is removed.
    pub fn next_message(&mut self) -> Option<Vec<String>> {
        if self.buffer.len() < 4 {
            return None;
        }

        let length = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;

        if length > MAX_BODY_LEN {
            self.error = Some(DecodeError::FrameTooLarge {
                length,
                limit: MAX_BODY_LEN,
            });
            self.buffer.clear();
            return None;
        }

        let frame_len = 4 + length;
        if self.buffer.len() < frame_len {
            return None;
        }

        let body: Vec<u8> = self.buffer.drain(..frame_len).collect();
        if std::str::from_utf8(&body).is_err() {
            self.error = Some(DecodeError::InvalidUtf8);
        }

        let text = String::from_utf8_lossy(&body);
        let mut fields: Vec<String> = text.split('\0').map(ToOwned::to_owned).collect();
        if fields.last().is_some_and(|last| last.is_empty()) {
            fields.pop();
        }
        Some(fields)
    }

    /// Takes the most recent protocol error, if any.
    ///
    /// Only one error is retained at a time; a later violation replaces an
    /// earlier one that has not been collected yet.
    pub fn take_error(&mut self) -> Option<DecodeError> {
        self.error.take()
    }

    /// Number of buffered bytes not yet consumed by a complete message.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        encode_message(&["4", "2", "-1", "326", "cliente ocupado", ""])
    }

    #[test]
    fn round_trip_preserves_every_field_including_empty_ones() {
        let fields = ["4", "2", "-1", "326", "client id in use", ""];
        let frame = encode_message(&fields);
        let body_len: u32 = fields.iter().map(|field| field.len() as u32 + 1).sum();
        assert_eq!(&frame[..4], &body_len.to_be_bytes()[..]);

        let mut decoder = Decoder::new();
        decoder.push(&frame);
        assert_eq!(
            decoder.next_message(),
            Some(vec![
                "4".into(),
                "2".into(),
                "-1".into(),
                "326".into(),
                "client id in use".into(),
                "".into(),
            ])
        );
        assert_eq!(decoder.next_message(), None);
        assert_eq!(decoder.take_error(), None);
    }

    #[test]
    fn round_trip_with_an_empty_body_yields_no_fields() {
        let mut decoder = Decoder::new();
        decoder.push(&encode_message(&[]));
        assert_eq!(decoder.next_message(), Some(Vec::new()));
    }

    #[test]
    fn message_split_into_arbitrary_chunks_waits_for_the_rest() {
        let frame = sample();
        let mut decoder = Decoder::new();
        let mut got = None;

        for (index, byte) in frame.iter().enumerate() {
            decoder.push(&[*byte]);
            if index + 1 < frame.len() {
                assert_eq!(decoder.next_message(), None, "frame completed too early");
            } else {
                got = decoder.next_message();
            }
        }

        assert_eq!(
            got,
            Some(vec![
                "4".into(),
                "2".into(),
                "-1".into(),
                "326".into(),
                "cliente ocupado".into(),
                "".into(),
            ])
        );
    }

    #[test]
    fn two_messages_in_one_push_are_returned_one_at_a_time() {
        let mut decoder = Decoder::new();
        let mut both = encode_message(&["193", "20261105 03:55:06 CET"]);
        both.extend_from_slice(&sample());
        decoder.push(&both);

        assert_eq!(
            decoder.next_message(),
            Some(vec!["193".into(), "20261105 03:55:06 CET".into()])
        );
        assert_eq!(
            decoder.next_message(),
            Some(vec![
                "4".into(),
                "2".into(),
                "-1".into(),
                "326".into(),
                "cliente ocupado".into(),
                "".into(),
            ])
        );
        assert_eq!(decoder.next_message(), None);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn corrupt_length_is_a_protocol_error_and_discards_the_buffer() {
        let mut decoder = Decoder::new();
        let mut corrupt = ((MAX_BODY_LEN + 1) as u32).to_be_bytes().to_vec();
        corrupt.extend_from_slice(&[1, 2, 3]);
        decoder.push(&corrupt);

        assert_eq!(decoder.next_message(), None);
        assert_eq!(
            decoder.take_error(),
            Some(DecodeError::FrameTooLarge {
                length: MAX_BODY_LEN + 1,
                limit: MAX_BODY_LEN,
            })
        );
        assert_eq!(decoder.buffered_len(), 0);

        // The decoder can be reused after the poisoned buffer was dropped.
        decoder.push(&encode_message(&["9", "1", "42"]));
        assert_eq!(
            decoder.next_message(),
            Some(vec!["9".into(), "1".into(), "42".into()])
        );
        assert_eq!(decoder.take_error(), None);
    }

    #[test]
    fn invalid_utf8_is_delivered_lossily_and_reported() {
        let mut body = b"9\0".to_vec();
        body.push(0xff);
        body.push(0);
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);

        let mut decoder = Decoder::new();
        decoder.push(&frame);
        assert_eq!(
            decoder.next_message(),
            Some(vec!["9".into(), "\u{fffd}".into()])
        );
        assert_eq!(decoder.take_error(), Some(DecodeError::InvalidUtf8));
        assert_eq!(decoder.take_error(), None);
    }
}
