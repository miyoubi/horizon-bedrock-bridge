//! NetherNet data-channel message framing: a message is split into segments,
//! each prefixed by the number of segments still to come (0 on the last). The
//! reliable channel reassembles them in order; the unreliable channel carries at
//! most one segment.

use crate::error::{Error, Result};
use crate::nethernet::MAX_MESSAGE_SIZE;

/// A message may not grow past 256 segments (the count is a `u8`).
const MAX_REASSEMBLED_SIZE: usize = 256 * MAX_MESSAGE_SIZE;

/// One data-channel payload: the count still to come, then the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSegment {
    /// Zero on the final segment of a message.
    pub remaining_segments: u8,
    pub data: Vec<u8>,
}

impl MessageSegment {
    pub fn new(remaining_segments: u8, data: Vec<u8>) -> Self {
        Self {
            remaining_segments,
            data,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.data.len());
        out.push(self.remaining_segments);
        out.extend_from_slice(&self.data);
        out
    }

    /// Fails below two bytes, so a segment with an empty body is invalid.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(Error::decode(format!(
                "nethernet segment too short: {} bytes",
                data.len()
            )));
        }
        Ok(Self {
            remaining_segments: data[0],
            data: data[1..].to_vec(),
        })
    }
}

/// Reassembly buffer for one reliable message at a time.
#[derive(Clone, Debug, Default)]
pub struct Message {
    expected_segments: u8,
    data: Vec<u8>,
}

impl Message {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one segment. Returns the whole message on its final segment; an
    /// out-of-sequence count clears the buffer and errors.
    pub fn add_segment(&mut self, segment: MessageSegment) -> Result<Option<Vec<u8>>> {
        if self.expected_segments == 0 && segment.remaining_segments > 0 {
            self.expected_segments = segment
                .remaining_segments
                .checked_add(1)
                .ok_or_else(|| Error::decode("nethernet segment count out of range"))?;
        }

        if self.expected_segments > 0 {
            let expected_remaining = self.expected_segments - 1;
            if expected_remaining != segment.remaining_segments {
                self.data.clear();
                self.expected_segments = 0;
                return Err(Error::decode(format!(
                    "nethernet segment out of sequence: expected {}, got {}",
                    expected_remaining, segment.remaining_segments
                )));
            }
            self.expected_segments -= 1;
        }

        if self.data.len() + segment.data.len() > MAX_REASSEMBLED_SIZE {
            self.data = Vec::new();
            self.expected_segments = 0;
            return Err(Error::other("nethernet message exceeds the reassembly cap"));
        }
        self.data.extend_from_slice(&segment.data);

        if segment.remaining_segments == 0 {
            let data = std::mem::take(&mut self.data);
            self.expected_segments = 0;
            Ok(Some(data))
        } else {
            Ok(None)
        }
    }

    /// Split a message into segments (a single segment under the size cap).
    pub fn split_into_segments(data: &[u8]) -> Result<Vec<MessageSegment>> {
        let len = data.len();
        if len <= MAX_MESSAGE_SIZE {
            return Ok(vec![MessageSegment::new(0, data.to_vec())]);
        }
        let segment_count = len.div_ceil(MAX_MESSAGE_SIZE);
        if segment_count > 255 {
            return Err(Error::other(
                "nethernet message needs more than 255 segments",
            ));
        }
        let mut segments = Vec::with_capacity(segment_count);
        let mut left = segment_count as u8;
        for chunk in data.chunks(MAX_MESSAGE_SIZE) {
            left -= 1;
            segments.push(MessageSegment::new(left, chunk.to_vec()));
        }
        Ok(segments)
    }

    /// The unreliable channel carries one segment only.
    pub fn encode_unreliable(data: &[u8]) -> Result<Vec<u8>> {
        if data.len() > MAX_MESSAGE_SIZE.saturating_sub(1) {
            return Err(Error::other("nethernet unreliable message too large"));
        }
        Ok(MessageSegment::new(0, data.to_vec()).encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_segment_round_trips() {
        let data = b"hello, nethernet".to_vec();
        let segments = Message::split_into_segments(&data).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].remaining_segments, 0);
        let mut m = Message::new();
        assert_eq!(m.add_segment(segments[0].clone()).unwrap(), Some(data));
    }

    #[test]
    fn multi_segment_counts_descend_and_reassemble() {
        let data = vec![0x5au8; MAX_MESSAGE_SIZE * 2 + 100];
        let segments = Message::split_into_segments(&data).unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments
                .iter()
                .map(|s| s.remaining_segments)
                .collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        let mut m = Message::new();
        let mut out = None;
        for s in &segments {
            out = m.add_segment(s.clone()).unwrap().or(out);
        }
        assert_eq!(out, Some(data));
    }

    #[test]
    fn out_of_order_segment_clears_and_errors() {
        let data = vec![1u8; MAX_MESSAGE_SIZE * 2 + 100];
        let segments = Message::split_into_segments(&data).unwrap();
        let mut m = Message::new();
        assert!(m.add_segment(segments[1].clone()).unwrap().is_none());
        assert!(m.add_segment(segments[0].clone()).is_err());
    }

    #[test]
    fn oversized_remaining_count_is_rejected_not_overflowed() {
        let mut m = Message::new();
        assert!(
            m.add_segment(MessageSegment::new(u8::MAX, vec![1]))
                .is_err()
        );
    }

    #[test]
    fn segment_encode_decode() {
        let s = MessageSegment::new(1, vec![9, 9]);
        assert_eq!(MessageSegment::decode(&s.encode()).unwrap(), s);
        assert!(MessageSegment::decode(&[0]).is_err());
    }

    #[test]
    fn unreliable_is_single_segment() {
        let encoded = Message::encode_unreliable(b"x").unwrap();
        assert_eq!(encoded[0], 0);
        assert!(Message::encode_unreliable(&vec![0u8; MAX_MESSAGE_SIZE]).is_err());
    }
}
