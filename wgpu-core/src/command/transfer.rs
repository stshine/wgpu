#[cfg(feature = "trace")]
use crate::device::trace::Command as TraceCommand;
use crate::{
    command::{
        collect_zero_buffer_copies_for_clear_texture, memory_init::fixup_discarded_surfaces,
        CommandBuffer, CommandEncoderError,
    },
    conv,
    device::Device,
    error::{ErrorFormatter, PrettyError},
    hub::{Global, GlobalIdentityHandlerFactory, HalApi, Storage, Token},
    id::{BufferId, CommandEncoderId, TextureId},
    init_tracker::{MemoryInitKind, TextureInitRange, TextureInitTrackerAction},
    resource::{Texture, TextureErrorDimension},
    track::TextureSelector,
};

use hal::CommandEncoder as _;
use thiserror::Error;
use wgt::{BufferAddress, BufferUsages, Extent3d, TextureUsages};

use std::iter;

pub type ImageCopyBuffer = wgt::ImageCopyBuffer<BufferId>;
pub type ImageCopyTexture = wgt::ImageCopyTexture<TextureId>;

#[derive(Clone, Debug)]
pub enum CopySide {
    Source,
    Destination,
}

/// Error encountered while attempting a data transfer.
#[derive(Clone, Debug, Error)]
pub enum TransferError {
    #[error("buffer {0:?} is invalid or destroyed")]
    InvalidBuffer(BufferId),
    #[error("texture {0:?} is invalid or destroyed")]
    InvalidTexture(TextureId),
    #[error("Source and destination cannot be the same buffer")]
    SameSourceDestinationBuffer,
    #[error("source buffer/texture is missing the `COPY_SRC` usage flag")]
    MissingCopySrcUsageFlag,
    #[error("destination buffer/texture is missing the `COPY_DST` usage flag")]
    MissingCopyDstUsageFlag(Option<BufferId>, Option<TextureId>),
    #[error("copy of {start_offset}..{end_offset} would end up overrunning the bounds of the {side:?} buffer of size {buffer_size}")]
    BufferOverrun {
        start_offset: BufferAddress,
        end_offset: BufferAddress,
        buffer_size: BufferAddress,
        side: CopySide,
    },
    #[error("copy of {dimension:?} {start_offset}..{end_offset} would end up overrunning the bounds of the {side:?} texture of {dimension:?} size {texture_size}")]
    TextureOverrun {
        start_offset: u32,
        end_offset: u32,
        texture_size: u32,
        dimension: TextureErrorDimension,
        side: CopySide,
    },
    #[error("unable to select texture aspect {aspect:?} from fromat {format:?}")]
    InvalidTextureAspect {
        format: wgt::TextureFormat,
        aspect: wgt::TextureAspect,
    },
    #[error("unable to select texture mip level {level} out of {total}")]
    InvalidTextureMipLevel { level: u32, total: u32 },
    #[error("buffer offset {0} is not aligned to block size or `COPY_BUFFER_ALIGNMENT`")]
    UnalignedBufferOffset(BufferAddress),
    #[error("copy size {0} does not respect `COPY_BUFFER_ALIGNMENT`")]
    UnalignedCopySize(BufferAddress),
    #[error("copy width is not a multiple of block width")]
    UnalignedCopyWidth,
    #[error("copy height is not a multiple of block height")]
    UnalignedCopyHeight,
    #[error("copy origin's x component is not a multiple of block width")]
    UnalignedCopyOriginX,
    #[error("copy origin's y component is not a multiple of block height")]
    UnalignedCopyOriginY,
    #[error("bytes per row does not respect `COPY_BYTES_PER_ROW_ALIGNMENT`")]
    UnalignedBytesPerRow,
    #[error("number of bytes per row needs to be specified since more than one row is copied")]
    UnspecifiedBytesPerRow,
    #[error("number of rows per image needs to be specified since more than one image is copied")]
    UnspecifiedRowsPerImage,
    #[error("number of bytes per row is less than the number of bytes in a complete row")]
    InvalidBytesPerRow,
    #[error("image is 1D and the copy height and depth are not both set to 1")]
    InvalidCopySize,
    #[error("number of rows per image is invalid")]
    InvalidRowsPerImage,
    #[error("source and destination layers have different aspects")]
    MismatchedAspects,
    #[error("copying from textures with format {0:?} is forbidden")]
    CopyFromForbiddenTextureFormat(wgt::TextureFormat),
    #[error("copying to textures with format {0:?} is forbidden")]
    CopyToForbiddenTextureFormat(wgt::TextureFormat),
    #[error("the entire texture must be copied when copying from depth texture")]
    InvalidDepthTextureExtent,
    #[error(
        "source format ({src_format:?}) and destination format ({dst_format:?}) are different"
    )]
    MismatchedTextureFormats {
        src_format: wgt::TextureFormat,
        dst_format: wgt::TextureFormat,
    },
}

impl PrettyError for TransferError {
    fn fmt_pretty(&self, fmt: &mut ErrorFormatter) {
        fmt.error(self);
        match *self {
            Self::InvalidBuffer(id) => {
                fmt.buffer_label(&id);
            }
            Self::InvalidTexture(id) => {
                fmt.texture_label(&id);
            }
            // Self::MissingCopySrcUsageFlag(buf_opt, tex_opt) => {
            //     if let Some(buf) = buf_opt {
            //         let name = crate::gfx_select!(buf => global.buffer_label(buf));
            //         ret.push_str(&format_label_line("source", &name));
            //     }
            //     if let Some(tex) = tex_opt {
            //         let name = crate::gfx_select!(tex => global.texture_label(tex));
            //         ret.push_str(&format_label_line("source", &name));
            //     }
            // }
            Self::MissingCopyDstUsageFlag(buf_opt, tex_opt) => {
                if let Some(buf) = buf_opt {
                    fmt.buffer_label_with_key(&buf, "destination");
                }
                if let Some(tex) = tex_opt {
                    fmt.texture_label_with_key(&tex, "destination");
                }
            }
            _ => {}
        };
    }
}
/// Error encountered while attempting to do a copy on a command encoder.
#[derive(Clone, Debug, Error)]
pub enum CopyError {
    #[error(transparent)]
    Encoder(#[from] CommandEncoderError),
    #[error("Copy error")]
    Transfer(#[from] TransferError),
}

pub(crate) fn extract_texture_selector<A: hal::Api>(
    copy_texture: &ImageCopyTexture,
    copy_size: &Extent3d,
    texture_guard: &Storage<Texture<A>, TextureId>,
) -> Result<(TextureSelector, hal::TextureCopyBase, wgt::TextureFormat), TransferError> {
    let texture = texture_guard
        .get(copy_texture.texture)
        .map_err(|_| TransferError::InvalidTexture(copy_texture.texture))?;

    let format = texture.desc.format;
    let copy_aspect =
        hal::FormatAspects::from(format) & hal::FormatAspects::from(copy_texture.aspect);
    if copy_aspect.is_empty() {
        return Err(TransferError::InvalidTextureAspect {
            format,
            aspect: copy_texture.aspect,
        });
    }

    let (layers, origin_z) = match texture.desc.dimension {
        wgt::TextureDimension::D1 | wgt::TextureDimension::D2 => (
            copy_texture.origin.z..copy_texture.origin.z + copy_size.depth_or_array_layers,
            0,
        ),
        wgt::TextureDimension::D3 => (0..1, copy_texture.origin.z),
    };
    let base = hal::TextureCopyBase {
        origin: wgt::Origin3d {
            x: copy_texture.origin.x,
            y: copy_texture.origin.y,
            z: origin_z,
        },
        // this value will be incremented per copied layer
        array_layer: layers.start,
        mip_level: copy_texture.mip_level,
        aspect: copy_aspect,
    };
    let selector = TextureSelector {
        levels: copy_texture.mip_level..copy_texture.mip_level + 1,
        layers,
    };

    Ok((selector, base, format))
}

/// Function copied with some modifications from webgpu standard <https://gpuweb.github.io/gpuweb/#copy-between-buffer-texture>
/// If successful, returns (number of buffer bytes required for this copy, number of bytes between array layers).
pub(crate) fn validate_linear_texture_data(
    layout: &wgt::ImageDataLayout,
    format: wgt::TextureFormat,
    buffer_size: BufferAddress,
    buffer_side: CopySide,
    bytes_per_block: BufferAddress,
    copy_size: &Extent3d,
    need_copy_aligned_rows: bool,
) -> Result<(BufferAddress, BufferAddress), TransferError> {
    // Convert all inputs to BufferAddress (u64) to prevent overflow issues
    let copy_width = copy_size.width as BufferAddress;
    let copy_height = copy_size.height as BufferAddress;
    let copy_depth = copy_size.depth_or_array_layers as BufferAddress;

    let offset = layout.offset;

    let (block_width, block_height) = format.describe().block_dimensions;
    let block_width = block_width as BufferAddress;
    let block_height = block_height as BufferAddress;
    let block_size = bytes_per_block;

    let width_in_blocks = copy_width / block_width;
    let height_in_blocks = copy_height / block_height;

    let bytes_per_row = if let Some(bytes_per_row) = layout.bytes_per_row {
        bytes_per_row.get() as BufferAddress
    } else {
        if copy_depth > 1 || height_in_blocks > 1 {
            return Err(TransferError::UnspecifiedBytesPerRow);
        }
        bytes_per_block * width_in_blocks
    };
    let block_rows_per_image = if let Some(rows_per_image) = layout.rows_per_image {
        rows_per_image.get() as BufferAddress
    } else {
        if copy_depth > 1 {
            return Err(TransferError::UnspecifiedRowsPerImage);
        }
        copy_height / block_height
    };
    let rows_per_image = block_rows_per_image * block_height;

    if copy_width % block_width != 0 {
        return Err(TransferError::UnalignedCopyWidth);
    }
    if copy_height % block_height != 0 {
        return Err(TransferError::UnalignedCopyHeight);
    }

    if need_copy_aligned_rows {
        let bytes_per_row_alignment = wgt::COPY_BYTES_PER_ROW_ALIGNMENT as BufferAddress;

        if bytes_per_row_alignment % bytes_per_block != 0 {
            return Err(TransferError::UnalignedBytesPerRow);
        }
        if bytes_per_row % bytes_per_row_alignment != 0 {
            return Err(TransferError::UnalignedBytesPerRow);
        }
    }

    let bytes_in_last_row = block_size * width_in_blocks;
    let bytes_per_image = bytes_per_row * block_rows_per_image;
    let required_bytes_in_copy = if copy_width == 0 || copy_height == 0 || copy_depth == 0 {
        0
    } else {
        let bytes_in_last_slice = bytes_per_row * (height_in_blocks - 1) + bytes_in_last_row;
        bytes_per_image * (copy_depth - 1) + bytes_in_last_slice
    };

    if rows_per_image < copy_height {
        return Err(TransferError::InvalidRowsPerImage);
    }
    if offset + required_bytes_in_copy > buffer_size {
        return Err(TransferError::BufferOverrun {
            start_offset: offset,
            end_offset: offset + required_bytes_in_copy,
            buffer_size,
            side: buffer_side,
        });
    }
    if offset % block_size != 0 {
        return Err(TransferError::UnalignedBufferOffset(offset));
    }
    if copy_height > 1 && bytes_per_row < bytes_in_last_row {
        return Err(TransferError::InvalidBytesPerRow);
    }
    Ok((required_bytes_in_copy, bytes_per_image))
}

/// Function copied with minor modifications from webgpu standard <https://gpuweb.github.io/gpuweb/#valid-texture-copy-range>
/// Returns the HAL copy extent and the layer count.
pub(crate) fn validate_texture_copy_range(
    texture_copy_view: &ImageCopyTexture,
    desc: &wgt::TextureDescriptor<()>,
    texture_side: CopySide,
    copy_size: &Extent3d,
) -> Result<(hal::CopyExtent, u32), TransferError> {
    let (block_width, block_height) = desc.format.describe().block_dimensions;
    let block_width = block_width as u32;
    let block_height = block_height as u32;

    let extent_virtual = desc.mip_level_size(texture_copy_view.mip_level).ok_or(
        TransferError::InvalidTextureMipLevel {
            level: texture_copy_view.mip_level,
            total: desc.mip_level_count,
        },
    )?;
    // physical size can be larger than the virtual
    let extent = extent_virtual.physical_size(desc.format);

    match desc.format {
        wgt::TextureFormat::Depth32Float
        | wgt::TextureFormat::Depth24Plus
        | wgt::TextureFormat::Depth24PlusStencil8 => {
            if *copy_size != extent {
                return Err(TransferError::InvalidDepthTextureExtent);
            }
        }
        _ => {}
    }

    let x_copy_max = texture_copy_view.origin.x + copy_size.width;
    if x_copy_max > extent.width {
        return Err(TransferError::TextureOverrun {
            start_offset: texture_copy_view.origin.x,
            end_offset: x_copy_max,
            texture_size: extent.width,
            dimension: TextureErrorDimension::X,
            side: texture_side,
        });
    }
    let y_copy_max = texture_copy_view.origin.y + copy_size.height;
    if y_copy_max > extent.height {
        return Err(TransferError::TextureOverrun {
            start_offset: texture_copy_view.origin.y,
            end_offset: y_copy_max,
            texture_size: extent.height,
            dimension: TextureErrorDimension::Y,
            side: texture_side,
        });
    }
    let z_copy_max = texture_copy_view.origin.z + copy_size.depth_or_array_layers;
    if z_copy_max > extent.depth_or_array_layers {
        return Err(TransferError::TextureOverrun {
            start_offset: texture_copy_view.origin.z,
            end_offset: z_copy_max,
            texture_size: extent.depth_or_array_layers,
            dimension: TextureErrorDimension::Z,
            side: texture_side,
        });
    }

    if texture_copy_view.origin.x % block_width != 0 {
        return Err(TransferError::UnalignedCopyOriginX);
    }
    if texture_copy_view.origin.y % block_height != 0 {
        return Err(TransferError::UnalignedCopyOriginY);
    }
    if copy_size.width % block_width != 0 {
        return Err(TransferError::UnalignedCopyWidth);
    }
    if copy_size.height % block_height != 0 {
        return Err(TransferError::UnalignedCopyHeight);
    }

    let (depth, array_layer_count) = match desc.dimension {
        wgt::TextureDimension::D1 | wgt::TextureDimension::D2 => {
            (1, copy_size.depth_or_array_layers)
        }
        wgt::TextureDimension::D3 => (copy_size.depth_or_array_layers, 1),
    };

    let copy_extent = hal::CopyExtent {
        width: copy_size.width,
        height: copy_size.height,
        depth,
    };
    Ok((copy_extent, array_layer_count))
}

fn get_copy_dst_texture_init_requirement<A: HalApi>(
    texture: &Texture<A>,
    copy_texture: &wgt::ImageCopyTexture<TextureId>,
    copy_size: &Extent3d,
) -> TextureInitTrackerAction {
    // Attention: If we don't write full texture subresources, we need to a full clear first since we don't track subrects.
    let dst_init_kind = if copy_size.width == texture.desc.size.width
        && copy_size.height == texture.desc.size.height
    {
        MemoryInitKind::ImplicitlyInitialized
    } else {
        MemoryInitKind::NeedsInitializedMemory
    };
    TextureInitTrackerAction {
        id: copy_texture.texture,
        range: TextureInitRange {
            mip_range: copy_texture.mip_level..copy_texture.mip_level + 1,
            layer_range: copy_texture.origin.z
                ..(copy_texture.origin.z + copy_size.depth_or_array_layers),
        },
        kind: dst_init_kind,
    }
}

fn handle_src_texture_init<A: hal::Api>(
    cmd_buf: &mut CommandBuffer<A>,
    device: &Device<A>,
    source: &ImageCopyTexture,
    src_base: &hal::TextureCopyBase,
    copy_size: &Extent3d,
    texture_guard: &Storage<Texture<A>, TextureId>,
) {
    let immediate_src_init = cmd_buf.texture_memory_actions.register_init_action(
        &TextureInitTrackerAction {
            id: source.texture,
            range: TextureInitRange {
                mip_range: src_base.mip_level..src_base.mip_level + 1,
                layer_range: src_base.origin.z
                    ..(src_base.origin.z + copy_size.depth_or_array_layers),
            },
            kind: MemoryInitKind::NeedsInitializedMemory,
        },
        texture_guard,
    );
    if !immediate_src_init.is_empty() {
        let cmd_buf_raw = cmd_buf.encoder.open();
        fixup_discarded_surfaces(
            immediate_src_init.into_iter(),
            cmd_buf_raw,
            texture_guard,
            &mut cmd_buf.trackers.textures,
            device,
        );
    }
}

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn command_encoder_copy_buffer_to_buffer<A: HalApi>(
        &self,
        command_encoder_id: CommandEncoderId,
        source: BufferId,
        source_offset: BufferAddress,
        destination: BufferId,
        destination_offset: BufferAddress,
        size: BufferAddress,
    ) -> Result<(), CopyError> {
        profiling::scope!("copy_buffer_to_buffer", "CommandEncoder");

        if source == destination {
            return Err(TransferError::SameSourceDestinationBuffer.into());
        }
        let hub = A::hub(self);
        let mut token = Token::root();

        let (mut cmd_buf_guard, mut token) = hub.command_buffers.write(&mut token);
        let cmd_buf = CommandBuffer::get_encoder_mut(&mut *cmd_buf_guard, command_encoder_id)?;
        let (buffer_guard, _) = hub.buffers.read(&mut token);

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf.commands {
            list.push(TraceCommand::CopyBufferToBuffer {
                src: source,
                src_offset: source_offset,
                dst: destination,
                dst_offset: destination_offset,
                size,
            });
        }

        let (src_buffer, src_pending) = cmd_buf
            .trackers
            .buffers
            .use_replace(&*buffer_guard, source, (), hal::BufferUses::COPY_SRC)
            .map_err(TransferError::InvalidBuffer)?;
        let src_raw = src_buffer
            .raw
            .as_ref()
            .ok_or(TransferError::InvalidBuffer(source))?;
        if !src_buffer.usage.contains(BufferUsages::COPY_SRC) {
            return Err(TransferError::MissingCopySrcUsageFlag.into());
        }
        // expecting only a single barrier
        let src_barrier = src_pending
            .map(|pending| pending.into_hal(src_buffer))
            .next();

        let (dst_buffer, dst_pending) = cmd_buf
            .trackers
            .buffers
            .use_replace(&*buffer_guard, destination, (), hal::BufferUses::COPY_DST)
            .map_err(TransferError::InvalidBuffer)?;
        let dst_raw = dst_buffer
            .raw
            .as_ref()
            .ok_or(TransferError::InvalidBuffer(destination))?;
        if !dst_buffer.usage.contains(BufferUsages::COPY_DST) {
            return Err(TransferError::MissingCopyDstUsageFlag(Some(destination), None).into());
        }
        let dst_barrier = dst_pending
            .map(|pending| pending.into_hal(dst_buffer))
            .next();

        if size % wgt::COPY_BUFFER_ALIGNMENT != 0 {
            return Err(TransferError::UnalignedCopySize(size).into());
        }
        if source_offset % wgt::COPY_BUFFER_ALIGNMENT != 0 {
            return Err(TransferError::UnalignedBufferOffset(source_offset).into());
        }
        if destination_offset % wgt::COPY_BUFFER_ALIGNMENT != 0 {
            return Err(TransferError::UnalignedBufferOffset(destination_offset).into());
        }

        let source_end_offset = source_offset + size;
        let destination_end_offset = destination_offset + size;
        if source_end_offset > src_buffer.size {
            return Err(TransferError::BufferOverrun {
                start_offset: source_offset,
                end_offset: source_end_offset,
                buffer_size: src_buffer.size,
                side: CopySide::Source,
            }
            .into());
        }
        if destination_end_offset > dst_buffer.size {
            return Err(TransferError::BufferOverrun {
                start_offset: destination_offset,
                end_offset: destination_end_offset,
                buffer_size: dst_buffer.size,
                side: CopySide::Destination,
            }
            .into());
        }

        if size == 0 {
            log::trace!("Ignoring copy_buffer_to_buffer of size 0");
            return Ok(());
        }

        // Make sure source is initialized memory and mark dest as initialized.
        cmd_buf
            .buffer_memory_init_actions
            .extend(dst_buffer.initialization_status.create_action(
                destination,
                destination_offset..(destination_offset + size),
                MemoryInitKind::ImplicitlyInitialized,
            ));
        cmd_buf
            .buffer_memory_init_actions
            .extend(src_buffer.initialization_status.create_action(
                source,
                source_offset..(source_offset + size),
                MemoryInitKind::NeedsInitializedMemory,
            ));

        let region = hal::BufferCopy {
            src_offset: source_offset,
            dst_offset: destination_offset,
            size: wgt::BufferSize::new(size).unwrap(),
        };
        let cmd_buf_raw = cmd_buf.encoder.open();
        unsafe {
            cmd_buf_raw.transition_buffers(src_barrier.into_iter().chain(dst_barrier));
            cmd_buf_raw.copy_buffer_to_buffer(src_raw, dst_raw, iter::once(region));
        }
        Ok(())
    }

    pub fn command_encoder_copy_buffer_to_texture<A: HalApi>(
        &self,
        command_encoder_id: CommandEncoderId,
        source: &ImageCopyBuffer,
        destination: &ImageCopyTexture,
        copy_size: &Extent3d,
    ) -> Result<(), CopyError> {
        profiling::scope!("copy_buffer_to_texture", "CommandEncoder");

        let hub = A::hub(self);
        let mut token = Token::root();

        let (device_guard, mut token) = hub.devices.read(&mut token);
        let (mut cmd_buf_guard, mut token) = hub.command_buffers.write(&mut token);
        let cmd_buf = CommandBuffer::get_encoder_mut(&mut *cmd_buf_guard, command_encoder_id)?;
        let (buffer_guard, mut token) = hub.buffers.read(&mut token);
        let (texture_guard, _) = hub.textures.read(&mut token);

        let device = &device_guard[cmd_buf.device_id.value];

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf.commands {
            list.push(TraceCommand::CopyBufferToTexture {
                src: source.clone(),
                dst: destination.clone(),
                size: *copy_size,
            });
        }

        if copy_size.width == 0 || copy_size.height == 0 || copy_size.depth_or_array_layers == 0 {
            log::trace!("Ignoring copy_buffer_to_texture of size 0");
            return Ok(());
        }

        let (dst_range, dst_base, _) =
            extract_texture_selector(destination, copy_size, &*texture_guard)?;

        let (src_buffer, src_pending) = cmd_buf
            .trackers
            .buffers
            .use_replace(&*buffer_guard, source.buffer, (), hal::BufferUses::COPY_SRC)
            .map_err(TransferError::InvalidBuffer)?;
        let src_raw = src_buffer
            .raw
            .as_ref()
            .ok_or(TransferError::InvalidBuffer(source.buffer))?;
        if !src_buffer.usage.contains(BufferUsages::COPY_SRC) {
            return Err(TransferError::MissingCopySrcUsageFlag.into());
        }
        let src_barriers = src_pending.map(|pending| pending.into_hal(src_buffer));

        let (dst_texture, dst_pending) = cmd_buf
            .trackers
            .textures
            .use_replace(
                &*texture_guard,
                destination.texture,
                dst_range,
                hal::TextureUses::COPY_DST,
            )
            .unwrap();
        let dst_raw = dst_texture
            .inner
            .as_raw()
            .ok_or(TransferError::InvalidTexture(destination.texture))?;
        if !dst_texture.desc.usage.contains(TextureUsages::COPY_DST) {
            return Err(
                TransferError::MissingCopyDstUsageFlag(None, Some(destination.texture)).into(),
            );
        }
        let dst_barriers = dst_pending.map(|pending| pending.into_hal(dst_texture));

        let format_desc = dst_texture.desc.format.describe();
        let (hal_copy_size, array_layer_count) = validate_texture_copy_range(
            destination,
            &dst_texture.desc,
            CopySide::Destination,
            copy_size,
        )?;
        let (required_buffer_bytes_in_copy, bytes_per_array_layer) = validate_linear_texture_data(
            &source.layout,
            dst_texture.desc.format,
            src_buffer.size,
            CopySide::Source,
            format_desc.block_size as BufferAddress,
            copy_size,
            true,
        )?;

        if !conv::is_valid_copy_dst_texture_format(dst_texture.desc.format) {
            return Err(
                TransferError::CopyToForbiddenTextureFormat(dst_texture.desc.format).into(),
            );
        }

        cmd_buf
            .buffer_memory_init_actions
            .extend(src_buffer.initialization_status.create_action(
                source.buffer,
                source.layout.offset..(source.layout.offset + required_buffer_bytes_in_copy),
                MemoryInitKind::NeedsInitializedMemory,
            ));
        let mut dst_zero_buffer_copy_regions = Vec::new();
        for immediate_init in cmd_buf.texture_memory_actions.register_init_action(
            &get_copy_dst_texture_init_requirement(dst_texture, destination, copy_size),
            &texture_guard,
        ) {
            collect_zero_buffer_copies_for_clear_texture(
                &dst_texture.desc,
                device.alignments.buffer_copy_pitch.get() as u32,
                immediate_init.mip_level..(immediate_init.mip_level + 1),
                immediate_init.layer..(immediate_init.layer + 1),
                &mut dst_zero_buffer_copy_regions,
            );
        }

        let regions = (0..array_layer_count).map(|rel_array_layer| {
            let mut texture_base = dst_base.clone();
            texture_base.array_layer += rel_array_layer;
            let mut buffer_layout = source.layout;
            buffer_layout.offset += rel_array_layer as u64 * bytes_per_array_layer;
            hal::BufferTextureCopy {
                buffer_layout,
                texture_base,
                size: hal_copy_size,
            }
        });
        let cmd_buf_raw = cmd_buf.encoder.open();
        unsafe {
            cmd_buf_raw.transition_textures(dst_barriers);
            // potential dst buffer init (for previously discarded dst_texture + partial copy)
            if !dst_zero_buffer_copy_regions.is_empty() {
                cmd_buf_raw.copy_buffer_to_texture(
                    &device.zero_buffer,
                    dst_raw,
                    dst_zero_buffer_copy_regions.into_iter(),
                );
            }
            cmd_buf_raw.transition_buffers(src_barriers);
            cmd_buf_raw.copy_buffer_to_texture(src_raw, dst_raw, regions);
        }
        Ok(())
    }

    pub fn command_encoder_copy_texture_to_buffer<A: HalApi>(
        &self,
        command_encoder_id: CommandEncoderId,
        source: &ImageCopyTexture,
        destination: &ImageCopyBuffer,
        copy_size: &Extent3d,
    ) -> Result<(), CopyError> {
        profiling::scope!("copy_texture_to_buffer", "CommandEncoder");

        let hub = A::hub(self);
        let mut token = Token::root();

        let (device_guard, mut token) = hub.devices.read(&mut token);
        let (mut cmd_buf_guard, mut token) = hub.command_buffers.write(&mut token);
        let cmd_buf = CommandBuffer::get_encoder_mut(&mut *cmd_buf_guard, command_encoder_id)?;
        let (buffer_guard, mut token) = hub.buffers.read(&mut token);
        let (texture_guard, _) = hub.textures.read(&mut token);

        let device = &device_guard[cmd_buf.device_id.value];

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf.commands {
            list.push(TraceCommand::CopyTextureToBuffer {
                src: source.clone(),
                dst: destination.clone(),
                size: *copy_size,
            });
        }

        if copy_size.width == 0 || copy_size.height == 0 || copy_size.depth_or_array_layers == 0 {
            log::trace!("Ignoring copy_texture_to_buffer of size 0");
            return Ok(());
        }

        let (src_range, src_base, _) =
            extract_texture_selector(source, copy_size, &*texture_guard)?;

        // Handle src texture init *before* dealing with barrier transitions so we have an easier time inserting "immediate-inits" that may be required by prior discards in rare cases.
        handle_src_texture_init(
            cmd_buf,
            device,
            source,
            &src_base,
            copy_size,
            &texture_guard,
        );

        let (src_texture, src_pending) = cmd_buf
            .trackers
            .textures
            .use_replace(
                &*texture_guard,
                source.texture,
                src_range,
                hal::TextureUses::COPY_SRC,
            )
            .unwrap();
        let src_raw = src_texture
            .inner
            .as_raw()
            .ok_or(TransferError::InvalidTexture(source.texture))?;
        if !src_texture.desc.usage.contains(TextureUsages::COPY_SRC) {
            return Err(TransferError::MissingCopySrcUsageFlag.into());
        }
        let src_barriers = src_pending.map(|pending| pending.into_hal(src_texture));

        let (dst_buffer, dst_pending) = cmd_buf
            .trackers
            .buffers
            .use_replace(
                &*buffer_guard,
                destination.buffer,
                (),
                hal::BufferUses::COPY_DST,
            )
            .map_err(TransferError::InvalidBuffer)?;
        let dst_raw = dst_buffer
            .raw
            .as_ref()
            .ok_or(TransferError::InvalidBuffer(destination.buffer))?;
        if !dst_buffer.usage.contains(BufferUsages::COPY_DST) {
            return Err(
                TransferError::MissingCopyDstUsageFlag(Some(destination.buffer), None).into(),
            );
        }
        let dst_barriers = dst_pending.map(|pending| pending.into_hal(dst_buffer));

        let format_desc = src_texture.desc.format.describe();
        let (hal_copy_size, array_layer_count) =
            validate_texture_copy_range(source, &src_texture.desc, CopySide::Source, copy_size)?;
        let (required_buffer_bytes_in_copy, bytes_per_array_layer) = validate_linear_texture_data(
            &destination.layout,
            src_texture.desc.format,
            dst_buffer.size,
            CopySide::Destination,
            format_desc.block_size as BufferAddress,
            copy_size,
            true,
        )?;

        if !conv::is_valid_copy_src_texture_format(src_texture.desc.format) {
            return Err(
                TransferError::CopyFromForbiddenTextureFormat(src_texture.desc.format).into(),
            );
        }

        cmd_buf
            .buffer_memory_init_actions
            .extend(dst_buffer.initialization_status.create_action(
                destination.buffer,
                destination.layout.offset
                    ..(destination.layout.offset + required_buffer_bytes_in_copy),
                MemoryInitKind::ImplicitlyInitialized,
            ));

        let regions = (0..array_layer_count).map(|rel_array_layer| {
            let mut texture_base = src_base.clone();
            texture_base.array_layer += rel_array_layer;
            let mut buffer_layout = destination.layout;
            buffer_layout.offset += rel_array_layer as u64 * bytes_per_array_layer;
            hal::BufferTextureCopy {
                buffer_layout,
                texture_base,
                size: hal_copy_size,
            }
        });
        let cmd_buf_raw = cmd_buf.encoder.open();
        unsafe {
            cmd_buf_raw.transition_buffers(dst_barriers);
            cmd_buf_raw.transition_textures(src_barriers);
            cmd_buf_raw.copy_texture_to_buffer(
                src_raw,
                hal::TextureUses::COPY_SRC,
                dst_raw,
                regions,
            );
        }
        Ok(())
    }

    pub fn command_encoder_copy_texture_to_texture<A: HalApi>(
        &self,
        command_encoder_id: CommandEncoderId,
        source: &ImageCopyTexture,
        destination: &ImageCopyTexture,
        copy_size: &Extent3d,
    ) -> Result<(), CopyError> {
        profiling::scope!("copy_texture_to_texture", "CommandEncoder");

        let hub = A::hub(self);
        let mut token = Token::root();

        let (device_guard, mut token) = hub.devices.read(&mut token);
        let (mut cmd_buf_guard, mut token) = hub.command_buffers.write(&mut token);
        let cmd_buf = CommandBuffer::get_encoder_mut(&mut *cmd_buf_guard, command_encoder_id)?;
        let (_, mut token) = hub.buffers.read(&mut token); // skip token
        let (texture_guard, _) = hub.textures.read(&mut token);

        let device = &device_guard[cmd_buf.device_id.value];

        #[cfg(feature = "trace")]
        if let Some(ref mut list) = cmd_buf.commands {
            list.push(TraceCommand::CopyTextureToTexture {
                src: source.clone(),
                dst: destination.clone(),
                size: *copy_size,
            });
        }

        if copy_size.width == 0 || copy_size.height == 0 || copy_size.depth_or_array_layers == 0 {
            log::trace!("Ignoring copy_texture_to_texture of size 0");
            return Ok(());
        }

        let (src_range, src_tex_base, _) =
            extract_texture_selector(source, copy_size, &*texture_guard)?;
        let (dst_range, dst_tex_base, _) =
            extract_texture_selector(destination, copy_size, &*texture_guard)?;
        if src_tex_base.aspect != dst_tex_base.aspect {
            return Err(TransferError::MismatchedAspects.into());
        }

        // Handle src texture init *before* dealing with barrier transitions so we have an easier time inserting "immediate-inits" that may be required by prior discards in rare cases.
        handle_src_texture_init(
            cmd_buf,
            device,
            source,
            &src_tex_base,
            copy_size,
            &texture_guard,
        );

        let (src_texture, src_pending) = cmd_buf
            .trackers
            .textures
            .use_replace(
                &*texture_guard,
                source.texture,
                src_range,
                hal::TextureUses::COPY_SRC,
            )
            .unwrap();
        let src_raw = src_texture
            .inner
            .as_raw()
            .ok_or(TransferError::InvalidTexture(source.texture))?;
        if !src_texture.desc.usage.contains(TextureUsages::COPY_SRC) {
            return Err(TransferError::MissingCopySrcUsageFlag.into());
        }

        //TODO: try to avoid this the collection. It's needed because both
        // `src_pending` and `dst_pending` try to hold `trackers.textures` mutably.
        let mut barriers = src_pending
            .map(|pending| pending.into_hal(src_texture))
            .collect::<Vec<_>>();

        let (dst_texture, dst_pending) = cmd_buf
            .trackers
            .textures
            .use_replace(
                &*texture_guard,
                destination.texture,
                dst_range,
                hal::TextureUses::COPY_DST,
            )
            .unwrap();
        let dst_raw = dst_texture
            .inner
            .as_raw()
            .ok_or(TransferError::InvalidTexture(destination.texture))?;
        if !dst_texture.desc.usage.contains(TextureUsages::COPY_DST) {
            return Err(
                TransferError::MissingCopyDstUsageFlag(None, Some(destination.texture)).into(),
            );
        }

        // src and dst texture format must be the same.
        let src_format = src_texture.desc.format;
        let dst_format = dst_texture.desc.format;

        if src_format != dst_format {
            return Err(TransferError::MismatchedTextureFormats {
                src_format,
                dst_format,
            }
            .into());
        }

        barriers.extend(dst_pending.map(|pending| pending.into_hal(dst_texture)));

        let (src_copy_size, array_layer_count) =
            validate_texture_copy_range(source, &src_texture.desc, CopySide::Source, copy_size)?;
        let (dst_copy_size, _) = validate_texture_copy_range(
            destination,
            &dst_texture.desc,
            CopySide::Destination,
            copy_size,
        )?;

        let mut dst_zero_buffer_copy_regions = Vec::new();
        for immediate_init in cmd_buf.texture_memory_actions.register_init_action(
            &get_copy_dst_texture_init_requirement(dst_texture, destination, copy_size),
            &texture_guard,
        ) {
            collect_zero_buffer_copies_for_clear_texture(
                &dst_texture.desc,
                device.alignments.buffer_copy_pitch.get() as u32,
                immediate_init.mip_level..(immediate_init.mip_level + 1),
                immediate_init.layer..(immediate_init.layer + 1),
                &mut dst_zero_buffer_copy_regions,
            );
        }

        let hal_copy_size = hal::CopyExtent {
            width: src_copy_size.width.min(dst_copy_size.width),
            height: src_copy_size.height.min(dst_copy_size.height),
            depth: src_copy_size.depth.min(dst_copy_size.depth),
        };
        let regions = (0..array_layer_count).map(|rel_array_layer| {
            let mut src_base = src_tex_base.clone();
            let mut dst_base = dst_tex_base.clone();
            src_base.array_layer += rel_array_layer;
            dst_base.array_layer += rel_array_layer;
            hal::TextureCopy {
                src_base,
                dst_base,
                size: hal_copy_size,
            }
        });
        let cmd_buf_raw = cmd_buf.encoder.open();
        unsafe {
            cmd_buf_raw.transition_textures(barriers.into_iter());

            // potential dst buffer init (for previously discarded dst_texture + partial copy)
            if !dst_zero_buffer_copy_regions.is_empty() {
                cmd_buf_raw.copy_buffer_to_texture(
                    &device.zero_buffer,
                    dst_raw,
                    dst_zero_buffer_copy_regions.into_iter(),
                );
            }

            cmd_buf_raw.copy_texture_to_texture(
                src_raw,
                hal::TextureUses::COPY_SRC,
                dst_raw,
                regions,
            );
        }
        Ok(())
    }
}
