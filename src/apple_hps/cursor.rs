use std::{collections::VecDeque, sync::Arc};

use flate2::{Decompress, FlushDecompress};

use crate::error::{Error, Result};

const MAX_DIMENSION: usize = 512;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Update {
    Image(Arc<Image>),
    Hidden,
}

fn update(image: Arc<Image>) -> Update {
    if image.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0) {
        Update::Image(image)
    } else {
        Update::Hidden
    }
}

#[derive(Default)]
pub struct Cache {
    entries: VecDeque<(u32, Arc<Image>)>,
    bytes: usize,
}

impl Cache {
    pub fn receive(
        &mut self,
        id: u32,
        rectangle: [u16; 4],
        compressed: &[u8],
    ) -> Result<Option<Update>> {
        if compressed.is_empty() {
            let Some(index) = self.entries.iter().position(|(cached, _)| *cached == id) else {
                return Ok(None);
            };
            let entry = self
                .entries
                .remove(index)
                .expect("known cursor cache entry");
            let selected = update(entry.1.clone());
            self.entries.push_back(entry);
            return Ok(Some(selected));
        }
        let [hotspot_x, hotspot_y, width, height] = rectangle;
        let pixels = usize::from(width) * usize::from(height);
        if usize::from(width) > MAX_DIMENSION
            || usize::from(height) > MAX_DIMENSION
            || (pixels > 0 && (hotspot_x >= width || hotspot_y >= height))
            || (width == 0) != (height == 0)
            || compressed.len() > 1024 * 1024
        {
            return Err(Error::Protocol(
                "Apple cursor exceeds its dimension or payload limit".into(),
            ));
        }
        let expected = pixels * 5;
        let mut raw = vec![0; expected + 1];
        let mut decoder = Decompress::new(true);
        decoder
            .decompress(compressed, &mut raw, FlushDecompress::Sync)
            .map_err(|_| Error::Protocol("Apple cursor has invalid compressed pixels".into()))?;
        if decoder.total_out() != expected as u64 || decoder.total_in() != compressed.len() as u64 {
            return Err(Error::Protocol(
                "Apple cursor pixel length is invalid".into(),
            ));
        }
        let mut rgba = Vec::with_capacity(pixels * 4);
        for index in 0..pixels {
            let offset = index * 4;
            rgba.extend_from_slice(&[
                raw[offset + 2],
                raw[offset + 1],
                raw[offset],
                raw[pixels * 4 + index],
            ]);
        }
        let image = Arc::new(Image {
            width,
            height,
            hotspot_x,
            hotspot_y,
            rgba,
        });
        if let Some(index) = self.entries.iter().position(|(cached, _)| *cached == id) {
            let (_, previous) = self
                .entries
                .remove(index)
                .expect("known cursor cache entry");
            self.bytes -= previous.rgba.len();
        }
        while self.entries.len() >= MAX_CACHE_ENTRIES
            || self.bytes + image.rgba.len() > MAX_CACHE_BYTES
        {
            let (_, previous) = self
                .entries
                .pop_front()
                .expect("nonempty bounded cursor cache");
            self.bytes -= previous.rgba.len();
        }
        self.bytes += image.rgba.len();
        self.entries.push_back((id, image.clone()));
        Ok(Some(update(image)))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::ZlibEncoder};

    use super::*;

    fn compressed(raw: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(raw).unwrap();
        encoder.flush().unwrap();
        encoder.get_ref().clone()
    }

    #[test]
    fn decodes_sync_flushed_bgra_and_alpha_without_losing_the_hotspot() {
        let raw = [3, 2, 1, 0, 6, 5, 4, 0, 255, 128];
        let mut cache = Cache::default();
        let Update::Image(image) = cache
            .receive(1000, [1, 0, 2, 1], &compressed(&raw))
            .unwrap()
            .unwrap()
        else {
            panic!("visible cursor")
        };
        assert_eq!(image.rgba, [1, 2, 3, 255, 4, 5, 6, 128]);
        assert_eq!((image.hotspot_x, image.hotspot_y), (1, 0));
        let Update::Image(cached) = cache.receive(1000, [0; 4], &[]).unwrap().unwrap() else {
            panic!("cached cursor")
        };
        assert!(Arc::ptr_eq(&image, &cached));
        assert!(cache.receive(1001, [0; 4], &[]).unwrap().is_none());
    }

    #[test]
    fn transparent_shapes_hide_the_cursor_and_cache_growth_is_bounded() {
        let mut cache = Cache::default();
        let pixel = compressed(&[0; 5]);
        for id in 0..100 {
            assert!(matches!(
                cache.receive(id, [0, 0, 1, 1], &pixel).unwrap(),
                Some(Update::Hidden)
            ));
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(cache.bytes <= MAX_CACHE_BYTES);
        assert!(cache.receive(0, [0; 4], &[]).unwrap().is_none());
    }

    #[test]
    fn malformed_and_oversized_shapes_are_rejected_before_caching() {
        let mut cache = Cache::default();
        assert!(cache.receive(1, [0, 0, 513, 1], &[1]).is_err());
        assert!(cache.receive(1, [1, 0, 1, 1], &[1]).is_err());
        assert!(
            cache
                .receive(1, [0, 0, 1, 1], &compressed(&[0; 10]))
                .is_err()
        );
        assert!(cache.receive(1, [0, 0, 1, 1], &[0; 3]).is_err());
        assert!(cache.entries.is_empty());
    }
}
