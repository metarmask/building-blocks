use crate::{prelude::*, ArrayIndexer, ArrayNx1};

use building_blocks_core::prelude::*;

pub trait ChunkDownsampler<N, T, Src> {
    /// Samples `src_chunk` in order to write out just a portion of `dst_chunk`, starting at `dst_min`.
    fn downsample(
        &self,
        src_chunk: &Src,
        dst_chunk: &mut ArrayNx1<N, T>,
        dst_min: Local<N>,
        level_delta: u8,
    );
}

/// A `ChunkDownsampler` that just selects a single point from each `2x2x2` region, discarding the rest.
pub struct PointDownsampler;

impl<N, Src, T> ChunkDownsampler<N, T, Src> for PointDownsampler
where
    N: ArrayIndexer<N>,
    PointN<N>: IntegerPoint<N>,
    T: 'static + Copy,
    Src: Get<Local<N>, Item = T> + IndexedArray<N>,
{
    fn downsample(
        &self,
        src_chunk: &Src,
        dst_chunk: &mut ArrayNx1<N, T>,
        dst_min: Local<N>,
        lod_delta: u8,
    ) {
        // PERF: this might be faster using Strides

        debug_assert!(lod_delta > 0);
        let lod_delta = lod_delta as i32;

        let dst_shape = src_chunk.extent().shape >> lod_delta;
        debug_assert!(dst_shape > PointN::ZERO);

        for p in ExtentN::from_min_and_shape(PointN::ZERO, dst_shape).iter_points() {
            *dst_chunk.get_mut(Local(dst_min.0 + p)) = src_chunk.get(Local(p << lod_delta));
        }
    }
}

/// A `ChunkDownsampler` that takes the mean of each `2x2x2` region of a signed distance field. It also renormalizes the values
/// to lie in the range `[-1.0, 1.0]`.
pub struct SdfMeanDownsampler;

impl<N, Src, T> ChunkDownsampler<N, T, Src> for SdfMeanDownsampler
where
    N: ArrayIndexer<N>,
    PointN<N>: IntegerPoint<N>,
    T: 'static + Clone + From<f32>,
    f32: From<T>,
    Src: Get<Local<N>, Item = T> + IndexedArray<N>,
{
    fn downsample(
        &self,
        src_chunk: &Src,
        dst_chunk: &mut ArrayNx1<N, T>,
        dst_min: Local<N>,
        lod_delta: u8,
    ) {
        // PERF: the access pattern here might not be very cache friendly

        debug_assert!(lod_delta > 0);
        let lod_delta = lod_delta as i32;

        let lod_scale_factor = 1 << lod_delta;
        let src_shape_per_point = PointN::fill(lod_scale_factor);
        // Not only do we get the mean signed distance value by dividing by the volume, but we also re-normalize by dividing
        // by the scale factor (the ratio between voxel edge lengths at the different resolutions).
        let rescale = 1.0 / (lod_scale_factor * src_shape_per_point.volume()) as f32;

        let dst_shape = src_chunk.extent().shape >> lod_delta;
        debug_assert!(dst_shape > PointN::ZERO);

        for p_dst in ExtentN::from_min_and_shape(PointN::ZERO, dst_shape).iter_points() {
            let src_min = p_dst << lod_delta;
            let src_extent = ExtentN::from_min_and_shape(src_min, src_shape_per_point);

            let mut sum = 0.0;
            for p_src in src_extent.iter_points() {
                sum += f32::from(src_chunk.get(Local(p_src)));
            }

            *dst_chunk.get_mut(Local(dst_min.0 + p_dst)) = T::from(rescale * sum);
        }
    }
}
