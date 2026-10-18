//! Framing properties: no input panics the decoder, every chunking of a
//! valid stream decodes to the same frames, and a valid stream with a few
//! bytes changed decodes exactly as far as a message parser would.

use proptest::prelude::*;

use crate::*;

const MAX_FRAME: u64 = 1024;

fn frame_type() -> impl Strategy<Value = FrameType> {
    prop::sample::select(FrameType::ALL.to_vec())
}

/// A payload length: small, or at the frame limit.
fn payload_len() -> impl Strategy<Value = usize> {
    prop_oneof![
        4 => 0..300usize,
        1 => (MAX_FRAME as usize - 2)..=(MAX_FRAME as usize),
    ]
}

fn frames() -> impl Strategy<Value = Vec<(FrameType, u32, Vec<u8>)>> {
    prop::collection::vec(
        (frame_type(), any::<u32>()).prop_flat_map(|(frame_type, channel)| {
            payload_len()
                .prop_flat_map(|len| prop::collection::vec(any::<u8>(), len))
                .prop_map(move |payload| (frame_type, channel, payload))
        }),
        0..12,
    )
}

fn encode_all(frames: &[(FrameType, u32, Vec<u8>)]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|(frame_type, channel, payload)| encode_frame(*frame_type, *channel, payload))
        .collect()
}

/// Random bytes, or a valid stream with up to four bytes replaced, so
/// the decoder meets frames as well as garbage.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => prop::collection::vec(any::<u8>(), 0..2048),
        2 => (frames(), prop::collection::vec((any::<usize>(), any::<u8>()), 0..4)).prop_map(
            |(frames, edits)| {
                let mut stream = encode_all(&frames);
                for (at, byte) in edits {
                    if !stream.is_empty() {
                        let at = at % stream.len();
                        stream[at] = byte;
                    }
                }
                stream
            }
        ),
    ]
}

/// Splits `bytes` at the given cut points, in order.
fn chunks(bytes: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
    let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    let mut start = 0;
    for cut in cuts.into_iter().chain([bytes.len()]) {
        out.push(bytes[start..cut].to_vec());
        start = cut;
    }
    out
}

/// Every frame the decoder yields before it stops or fails.
fn decode_all(chunks: &[Vec<u8>]) -> (Vec<(Header, Vec<u8>)>, Option<FrameError>) {
    let mut decoder = FrameDecoder::default();
    let mut frames = Vec::new();
    for chunk in chunks {
        decoder.push(chunk);
        loop {
            match decoder.next_frame(MAX_FRAME) {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => break,
                Err(err) => return (frames, Some(err)),
            }
        }
    }
    (frames, None)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("CLANKERMUX_FUZZ_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(512),
        ..ProptestConfig::default()
    })]

    #[test]
    fn any_bytes_decode_or_fail_without_panicking(
        bytes in bytes(),
        cuts in prop::collection::vec(any::<usize>(), 0..8),
    ) {
        let (frames, error) = decode_all(&chunks(&bytes, &cuts));
        for (header, payload) in &frames {
            prop_assert_eq!(header.length as usize, payload.len());
            prop_assert!(u64::from(header.length) <= MAX_FRAME);
        }
        // What the decoder yields is what a message parser yields.
        let mut pos = 0;
        for (header, payload) in &frames {
            let end = pos + HEADER_LEN + payload.len();
            let (parsed, parsed_payload) = parse_frame(&bytes[pos..end], MAX_FRAME).unwrap();
            prop_assert_eq!(&parsed, header);
            prop_assert_eq!(parsed_payload, payload.as_slice());
            pos = end;
        }
        if let Some(error) = error {
            // The failure is at the header that follows the last frame.
            let head: Option<&[u8; HEADER_LEN]> = bytes.get(pos..pos + HEADER_LEN).and_then(|h| h.try_into().ok());
            prop_assert_eq!(head.map(|h| parse_header(h, MAX_FRAME)), Some(Err(error)));
        }
        let _ = parse_frame(&bytes, MAX_FRAME);
    }

    #[test]
    fn a_valid_stream_decodes_the_same_under_every_chunking(
        frames in frames(),
        cuts in prop::collection::vec(any::<usize>(), 0..16),
    ) {
        let stream = encode_all(&frames);
        let (decoded, error) = decode_all(&chunks(&stream, &cuts));
        prop_assert_eq!(error, None);
        prop_assert_eq!(decoded.len(), frames.len());
        for ((header, payload), (frame_type, channel, expected)) in decoded.iter().zip(&frames) {
            prop_assert_eq!(header.frame_type, *frame_type);
            prop_assert_eq!(header.channel, *channel);
            prop_assert_eq!(payload, expected);
        }
    }

    #[test]
    fn headers_fail_as_specified(
        frame_type in frame_type(),
        channel in any::<u32>(),
        length in prop_oneof![0..=(MAX_FRAME as u32), any::<u32>()],
        bad_type in any::<u8>(),
    ) {
        let header = encode_header(frame_type, channel, length);
        let parsed = parse_header(&header, MAX_FRAME);
        if u64::from(length) > MAX_FRAME {
            prop_assert_eq!(parsed, Err(FrameError::TooLong { length, max_frame: MAX_FRAME }));
        } else {
            prop_assert_eq!(parsed, Ok(Header { frame_type, channel, length }));
            // A message with exactly `length` payload bytes parses; one
            // with any other count is a mismatch.
            let mut message = header.to_vec();
            message.extend(std::iter::repeat_n(0u8, length as usize));
            prop_assert_eq!(parse_frame(&message, MAX_FRAME).map(|(h, p)| (h, p.len())), Ok((Header { frame_type, channel, length }, length as usize)));
            let payload = (length as usize + 1) % 64;
            message.resize(HEADER_LEN + payload, 0);
            prop_assert_eq!(parse_frame(&message, MAX_FRAME), Err(FrameError::LengthMismatch { length, payload }));
        }
        // The flags byte is ignored; an unknown type is not.
        let mut flagged = header;
        flagged[1] = 0xff;
        prop_assert_eq!(parse_header(&flagged, u64::MAX).map(|h| h.frame_type), Ok(frame_type));
        if FrameType::from_u8(bad_type).is_none() {
            let mut unknown = header;
            unknown[0] = bad_type;
            prop_assert_eq!(parse_header(&unknown, u64::MAX), Err(FrameError::UnknownType(bad_type)));
        }
        prop_assert_eq!(parse_frame(&header[..HEADER_LEN - 1], MAX_FRAME), Err(FrameError::Short));
    }
}
