use crate::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, DevicePixels, PlatformAtlas,
    Point, Size, platform::AtlasTextureList,
};
use anyhow::Result;
use collections::FxHashMap;
use etagere::BucketedAtlasAllocator;
use parking_lot::Mutex;
use std::{borrow::Cow, ops, sync::Arc};

/// Sprite atlas for the web renderer; a port of `BladeAtlas` to wgpu.
///
/// Unlike blade, uploads go through `Queue::write_texture` at insertion time
/// rather than a staging-buffer belt flushed per frame: queued writes are
/// guaranteed to execute before the next `Queue::submit`, which `draw` issues
/// every frame.
pub(crate) struct WebGpuAtlas(Mutex<WebGpuAtlasState>);

struct WebGpuAtlasState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    storage: WebGpuAtlasStorage,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
}

pub(crate) struct WebGpuTextureInfo {
    pub(crate) raw_view: wgpu::TextureView,
}

impl WebGpuAtlas {
    pub(crate) fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        WebGpuAtlas(Mutex::new(WebGpuAtlasState {
            device,
            queue,
            storage: WebGpuAtlasStorage::default(),
            tiles_by_key: Default::default(),
        }))
    }

    pub(crate) fn get_texture_info(&self, id: AtlasTextureId) -> WebGpuTextureInfo {
        let lock = self.0.lock();
        let texture = &lock.storage[id];
        WebGpuTextureInfo {
            raw_view: texture.raw_view.clone(),
        }
    }
}

impl PlatformAtlas for WebGpuAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut lock = self.0.lock();
        if let Some(tile) = lock.tiles_by_key.get(key) {
            Ok(Some(tile.clone()))
        } else {
            let Some((size, bytes)) = build()? else {
                return Ok(None);
            };
            let tile = lock.allocate(size, key.texture_kind());
            lock.upload_texture(tile.texture_id, tile.bounds, &bytes);
            lock.tiles_by_key.insert(key.clone(), tile.clone());
            Ok(Some(tile))
        }
    }

    fn remove(&self, key: &AtlasKey) {
        let mut lock = self.0.lock();

        let Some(id) = lock.tiles_by_key.remove(key).map(|tile| tile.texture_id) else {
            return;
        };

        let Some(texture_slot) = lock.storage[id.kind].textures.get_mut(id.index as usize) else {
            return;
        };

        if let Some(mut texture) = texture_slot.take() {
            texture.decrement_ref_count();
            if texture.is_unreferenced() {
                lock.storage[id.kind]
                    .free_list
                    .push(texture.id.index as usize);
                texture.destroy();
            } else {
                *texture_slot = Some(texture);
            }
        }
    }
}

impl WebGpuAtlasState {
    fn allocate(&mut self, size: Size<DevicePixels>, texture_kind: AtlasTextureKind) -> AtlasTile {
        {
            let textures = &mut self.storage[texture_kind];

            if let Some(tile) = textures
                .iter_mut()
                .rev()
                .find_map(|texture| texture.allocate(size))
            {
                return tile;
            }
        }

        let texture = self.push_texture(size, texture_kind);
        texture.allocate(size).unwrap()
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> &mut WebGpuAtlasTexture {
        const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(1024),
            height: DevicePixels(1024),
        };

        let size = min_size.max(&DEFAULT_ATLAS_SIZE);
        // Same formats as `BladeAtlas`: non-sRGB, since gpui's shaders work in
        // sRGB-encoded values throughout.
        let format = match kind {
            AtlasTextureKind::Monochrome => wgpu::TextureFormat::R8Unorm,
            AtlasTextureKind::Polychrome => wgpu::TextureFormat::Bgra8Unorm,
        };

        let raw = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: size.width.into(),
                height: size.height.into(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let raw_view = raw.create_view(&wgpu::TextureViewDescriptor::default());

        let texture_list = &mut self.storage[kind];
        let index = texture_list.free_list.pop();

        let atlas_texture = WebGpuAtlasTexture {
            id: AtlasTextureId {
                index: index.unwrap_or(texture_list.textures.len()) as u32,
                kind,
            },
            allocator: BucketedAtlasAllocator::new(etagere::Size::new(
                size.width.into(),
                size.height.into(),
            )),
            format,
            raw,
            raw_view,
            live_atlas_keys: 0,
        };

        if let Some(ix) = index {
            texture_list.textures[ix] = Some(atlas_texture);
            texture_list.textures.get_mut(ix).unwrap().as_mut().unwrap()
        } else {
            texture_list.textures.push(Some(atlas_texture));
            texture_list.textures.last_mut().unwrap().as_mut().unwrap()
        }
    }

    fn upload_texture(&mut self, id: AtlasTextureId, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let texture = &self.storage[id];
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture.raw,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: bounds.origin.x.into(),
                    y: bounds.origin.y.into(),
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bounds.size.width.to_bytes(texture.bytes_per_pixel())),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: bounds.size.width.into(),
                height: bounds.size.height.into(),
                depth_or_array_layers: 1,
            },
        );
    }
}

#[derive(Default)]
struct WebGpuAtlasStorage {
    monochrome_textures: AtlasTextureList<WebGpuAtlasTexture>,
    polychrome_textures: AtlasTextureList<WebGpuAtlasTexture>,
}

impl ops::Index<AtlasTextureKind> for WebGpuAtlasStorage {
    type Output = AtlasTextureList<WebGpuAtlasTexture>;
    fn index(&self, kind: AtlasTextureKind) -> &Self::Output {
        match kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures,
            AtlasTextureKind::Polychrome => &self.polychrome_textures,
        }
    }
}

impl ops::IndexMut<AtlasTextureKind> for WebGpuAtlasStorage {
    fn index_mut(&mut self, kind: AtlasTextureKind) -> &mut Self::Output {
        match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
        }
    }
}

impl ops::Index<AtlasTextureId> for WebGpuAtlasStorage {
    type Output = WebGpuAtlasTexture;
    fn index(&self, id: AtlasTextureId) -> &Self::Output {
        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures,
            AtlasTextureKind::Polychrome => &self.polychrome_textures,
        };
        textures[id.index as usize].as_ref().unwrap()
    }
}

struct WebGpuAtlasTexture {
    id: AtlasTextureId,
    allocator: BucketedAtlasAllocator,
    raw: wgpu::Texture,
    raw_view: wgpu::TextureView,
    format: wgpu::TextureFormat,
    live_atlas_keys: u32,
}

impl WebGpuAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self
            .allocator
            .allocate(etagere::Size::new(size.width.into(), size.height.into()))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            padding: 0,
            bounds: Bounds {
                origin: Point {
                    x: DevicePixels::from(allocation.rectangle.min.x),
                    y: DevicePixels::from(allocation.rectangle.min.y),
                },
                size,
            },
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn destroy(&mut self) {
        self.raw.destroy();
    }

    fn bytes_per_pixel(&self) -> u8 {
        match self.format {
            wgpu::TextureFormat::R8Unorm => 1,
            _ => 4,
        }
    }

    fn decrement_ref_count(&mut self) {
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&mut self) -> bool {
        self.live_atlas_keys == 0
    }
}

// The renderer hands `Arc<WebGpuAtlas>` to gpui as `Arc<dyn PlatformAtlas>`.
const _: fn(Arc<WebGpuAtlas>) -> Arc<dyn PlatformAtlas> = |atlas| atlas;
