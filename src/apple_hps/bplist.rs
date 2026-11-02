use crate::error::{Error, Result};

const KEY: &[u8] = b"avcMediaStreamNegotiatorMediaBlob";

fn failed(message: &str) -> Error {
    Error::Protocol(format!("Apple HEVC property list: {message}"))
}

fn unsigned(bytes: &[u8]) -> Result<usize> {
    if bytes.is_empty() || bytes.len() > 8 {
        return Err(failed("invalid integer width"));
    }
    let value = bytes
        .iter()
        .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
    usize::try_from(value).map_err(|_| failed("integer exceeds the address space"))
}

struct Table<'a> {
    data: &'a [u8],
    offset: usize,
    count: usize,
    offset_width: usize,
    reference_width: usize,
    root: usize,
}

impl<'a> Table<'a> {
    fn object(&self, index: usize) -> Result<(u8, usize, usize)> {
        if index >= self.count {
            return Err(failed("invalid object reference"));
        }
        let entry = self.offset + index * self.offset_width;
        let offset = unsigned(&self.data[entry..entry + self.offset_width])?;
        if !(8..self.offset).contains(&offset) {
            return Err(failed("object lies outside its table"));
        }
        let marker = self.data[offset];
        let mut start = offset + 1;
        let mut length = usize::from(marker & 15);
        if length == 15 {
            let integer = *self
                .data
                .get(start)
                .ok_or_else(|| failed("missing extended length"))?;
            if integer >> 4 != 1 || integer & 15 > 3 {
                return Err(failed("invalid extended length"));
            }
            let size = 1_usize << (integer & 15);
            start += 1;
            let end = start
                .checked_add(size)
                .filter(|end| *end <= self.offset)
                .ok_or_else(|| failed("truncated extended length"))?;
            length = unsigned(&self.data[start..end])?;
            start = end;
        }
        Ok((marker >> 4, length, start))
    }

    fn range(&self, start: usize, length: usize) -> Result<&'a [u8]> {
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.offset)
            .ok_or_else(|| failed("object exceeds its payload"))?;
        Ok(&self.data[start..end])
    }

    fn key(&self, index: usize) -> Result<bool> {
        let (kind, length, start) = self.object(index)?;
        match kind {
            5 | 7 => Ok(self.range(start, length)? == KEY),
            6 => {
                let size = length
                    .checked_mul(2)
                    .ok_or_else(|| failed("string length overflow"))?;
                let value = self.range(start, size)?;
                Ok(length == KEY.len()
                    && value
                        .chunks_exact(2)
                        .zip(KEY)
                        .all(|(pair, expected)| pair == [0, *expected]))
            }
            _ => Err(failed("dictionary key is not a string")),
        }
    }

    fn media_blob(&self) -> Result<Option<&'a [u8]>> {
        let (kind, count, start) = self.object(self.root)?;
        if kind != 13 {
            return Ok(None);
        }
        if count > 256 {
            return Err(failed("too many media dictionary entries"));
        }
        let size = count
            .checked_mul(self.reference_width)
            .ok_or_else(|| failed("reference count overflow"))?;
        let references = self.range(start, size * 2)?;
        for index in 0..count {
            let offset = index * self.reference_width;
            let key = unsigned(&references[offset..offset + self.reference_width])?;
            if self.key(key)? {
                let offset = size + offset;
                let value = unsigned(&references[offset..offset + self.reference_width])?;
                let (kind, length, begin) = self.object(value)?;
                if kind != 4 {
                    return Err(failed("media blob is not binary data"));
                }
                return self.range(begin, length).map(Some);
            }
        }
        Ok(None)
    }
}

pub(super) fn media_blobs(data: &[u8]) -> Result<Vec<&[u8]>> {
    if data.len() > 65_535 {
        return Err(failed("media metadata exceeds its size limit"));
    }
    let mut blobs = Vec::new();
    let mut next = 0;
    for _ in 0..16 {
        let Some(relative) = data[next..]
            .windows(8)
            .position(|bytes| bytes == b"bplist00")
        else {
            return Ok(blobs);
        };
        let start = next + relative;
        let mut found = None;
        // Apple combines audio/video bplists and key blobs in one control payload.
        // Locate self-consistent trailers instead of reparsing every possible suffix.
        for trailer in start + 8..data.len().saturating_sub(31) {
            if data[trailer..trailer + 6] != [0; 6] {
                continue;
            }
            let offset_width = usize::from(data[trailer + 6]);
            let reference_width = usize::from(data[trailer + 7]);
            if !matches!(offset_width, 1 | 2 | 4 | 8) || !matches!(reference_width, 1 | 2 | 4 | 8) {
                continue;
            }
            let count = unsigned(&data[trailer + 8..trailer + 16])?;
            let root = unsigned(&data[trailer + 16..trailer + 24])?;
            let offset = unsigned(&data[trailer + 24..trailer + 32])?;
            if count == 0 || count > 4096 || root >= count || offset < 8 {
                continue;
            }
            let expected = offset.checked_add(count * offset_width);
            if expected != Some(trailer - start) {
                continue;
            }
            found = Some((
                trailer + 32,
                offset,
                count,
                offset_width,
                reference_width,
                root,
            ));
            break;
        }
        let (end, offset, count, offset_width, reference_width, root) =
            found.ok_or_else(|| failed("no valid bounded property-list trailer"))?;
        let table = Table {
            data: &data[start..end],
            offset,
            count,
            offset_width,
            reference_width,
            root,
        };
        if let Some(blob) = table.media_blob()? {
            blobs.push(blob);
        }
        next = end;
    }
    if data[next..].windows(8).any(|bytes| bytes == b"bplist00") {
        Err(failed("too many embedded property lists"))
    } else {
        Ok(blobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::{Dictionary, Value};

    fn plist(blob: &[u8]) -> Vec<u8> {
        let mut dictionary = Dictionary::new();
        dictionary.insert(
            String::from_utf8(KEY.to_vec()).unwrap(),
            Value::Data(blob.to_vec()),
        );
        let mut data = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_binary(&mut data)
            .unwrap();
        data
    }

    #[test]
    fn extracts_audio_and_video_from_combined_keyed_reply_without_expanding_graphs() {
        let mut data = vec![0; 128];
        data.extend_from_slice(&plist(b"audio"));
        data.extend_from_slice(&[17; 92]);
        data.extend_from_slice(&plist(b"video"));
        data.extend_from_slice(&[0; 12]);
        assert_eq!(
            media_blobs(&data).unwrap(),
            vec![b"audio".as_slice(), b"video".as_slice()]
        );
        assert!(media_blobs(&data[..140]).is_err());
        assert!(media_blobs(&vec![0; 65_536]).is_err());
    }
}
