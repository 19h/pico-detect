use std::io::{Error, ErrorKind, Read};

use image::{GenericImageView, Luma};
use imageproc::rect::Rect;
use nalgebra::{Affine2, Dynamic, OMatrix, Point2, Vector2, U2};

use super::bintest::FeatureBintest;
use super::geometry::{find_affine, find_similarity};
use super::node::ThresholdNode;
use super::utils::get_pixel_with_fallback;

pub type ShapeMatrix = OMatrix<f32, U2, Dynamic>;

struct Tree {
    nodes: Vec<ThresholdNode>,
    shifts: Vec<Vec<Vector2<f32>>>,
}

struct Forest {
    trees: Vec<Tree>,
    anchors: Vec<usize>,
    deltas: Vec<Vector2<f32>>,
}

impl Forest {
    #[inline]
    fn extract_feature_pixel_values<I>(
        &self,
        image: &I,
        transform_to_image: &Affine2<f32>,
        initial_shape: &[Point2<f32>],
        shape: &[Point2<f32>],
        features: &mut [u8],
    ) where
        I: GenericImageView<Pixel = Luma<u8>>,
    {
        debug_assert_eq!(self.deltas.len(), self.anchors.len());
        debug_assert_eq!(features.len(), self.anchors.len());

        let transform_to_shape =
            find_similarity(
                initial_shape,
                shape,
            );

        for ((delta, anchor), feature) in self
            .deltas
            .iter()
            .zip(self.anchors.iter())
            .zip(features.iter_mut())
        {
            let mut point = shape[*anchor] + transform_to_shape.transform_vector(delta);
            point = transform_to_image.transform_point(&point);

            *feature =
                get_pixel_with_fallback(image, point.x as i32, point.y as i32, Luma::from([0u8])).0
                    [0];
        }
    }
}

/// Implements object alignment using an ensemble of regression trees.
pub struct Shaper {
    initial_shape: Vec<Point2<f32>>,
    forests: Vec<Forest>,
    depth: usize,
    dsize: usize,
    features: Vec<u8>,
}

impl Shaper {
    /// Create a shaper object from a readable source.
    pub fn from_readable(mut readable: impl Read) -> Result<Self, Error> {
        let mut buf = [0u8; 4];
        readable.read_exact(&mut buf[0..1])?;
        let version = buf[0];
        if version != 1 {
            return Err(Error::new(ErrorKind::InvalidData, "wrong version"));
        }

        readable.read_exact(&mut buf)?;
        let nrows = u32::from_be_bytes(buf) as usize;

        readable.read_exact(&mut buf)?;
        let ncols = u32::from_be_bytes(buf) as usize;

        let size = nrows * ncols;

        readable.read_exact(&mut buf)?;
        let nforests = u32::from_be_bytes(buf) as usize;

        readable.read_exact(&mut buf)?;
        let forest_size = u32::from_be_bytes(buf) as usize;

        readable.read_exact(&mut buf)?;
        let tree_depth = u32::from_be_bytes(buf);

        readable.read_exact(&mut buf)?;
        let nfeatures = u32::from_be_bytes(buf) as usize;

        let leafs_count = 2u32.pow(tree_depth) as usize;
        let splits_count = leafs_count - 1;

        // dbg!(nrows, ncols, nforests, forest_size, tree_depth, nfeatures);
        let initial_shape: Vec<Point2<f32>> = shape_from_readable(readable.by_ref(), size)?
            .column_iter()
            .map(|col| Point2::new(col.x, col.y))
            .collect();

        let mut forests: Vec<Forest> = Vec::with_capacity(nforests);
        for _ in 0..nforests {
            let mut trees = Vec::with_capacity(forest_size);
            for _ in 0..forest_size {
                let mut nodes = Vec::with_capacity(splits_count);
                let mut buf10 = [0u8; 10];
                for _ in 0..splits_count {
                    readable.read_exact(&mut buf10)?;
                    nodes.push(ThresholdNode::from(buf10));
                }

                let mut shifts = Vec::with_capacity(leafs_count);
                for _ in 0..leafs_count {
                    let shift: Vec<Vector2<f32>> = shape_from_readable(readable.by_ref(), size)?
                        .column_iter()
                        .map(|col| Vector2::new(col.x, col.y))
                        .collect();
                    shifts.push(shift);
                }

                trees.push(Tree { nodes, shifts });
            }

            let mut anchors = Vec::with_capacity(nfeatures);
            for _ in 0..nfeatures {
                readable.read_exact(&mut buf)?;
                anchors.push(u32::from_be_bytes(buf) as usize);
            }

            let mut deltas = Vec::with_capacity(nfeatures);
            for _ in 0..nfeatures {
                readable.read_exact(&mut buf)?;
                let x = f32::from_be_bytes(buf);
                readable.read_exact(&mut buf)?;
                let y = f32::from_be_bytes(buf);
                deltas.push(Vector2::new(x, y));
            }

            forests.push(Forest {
                trees,
                anchors,
                deltas,
            });
        }

        Ok(Self {
            initial_shape,
            forests,
            depth: tree_depth as usize,
            dsize: splits_count,
            features: vec![0u8; nfeatures],
        })
    }

    /// Estimate object shape on the image
    ///
    /// ### Arguments
    ///
    /// * `image` - Target image.
    /// * `roi` - object location:
    ///   - `roi.x` position on image x-axis,
    ///   - `roi.y` position on image y-axis,
    ///   - `roi.z` object size.
    ///
    /// ### Returns
    ///
    /// A collection of points each one corresponds to landmark location.
    /// Points count is defined by a loaded shaper model.
    #[inline]
    pub fn predict<I>(&mut self, image: &I, rect: Rect) -> Vec<Point2<f32>>
    where
        I: GenericImageView<Pixel = Luma<u8>>,
    {
        let mut shape = self.initial_shape.clone();

        let transform_to_image = find_transform_to_image(rect);

        for forest in self.forests.iter() {
            forest.extract_feature_pixel_values(
                image,
                &transform_to_image,
                &self.initial_shape,
                &shape,
                self.features.as_mut(),
            );

            for tree in forest.trees.iter() {
                let idx = (0..self.depth).fold(0, |idx, _| {
                    2 * idx + 1 + tree.nodes[idx].bintest(&self.features) as usize
                }) - self.dsize;

                shape.iter_mut().zip(tree.shifts[idx].iter()).for_each(
                    |(shape_point, shift_vector)| {
                        *shape_point += shift_vector;
                    },
                );
            }
        }

        shape
            .iter_mut()
            .for_each(|point| *point = transform_to_image.transform_point(point));
        shape
    }
}

#[inline]
fn find_transform_to_image(rect: Rect) -> Affine2<f32> {
    let norm_corners = [
        Point2::new(0.0, 0.0),
        Point2::new(1.0, 0.0),
        Point2::new(1.0, 1.0),
    ];
    let (left, right, top, bottom) = (
        rect.left() as f32,
        rect.right() as f32,
        rect.top() as f32,
        rect.bottom() as f32,
    );

    let rect_corners = [
        Point2::new(left, top),
        Point2::new(right, top),
        Point2::new(right, bottom),
    ];
    find_affine(&norm_corners, &rect_corners, 0.0001).unwrap()
}

fn shape_from_readable(mut readable: impl Read, size: usize) -> Result<ShapeMatrix, Error> {
    let mut arr = Vec::with_capacity(size);
    let mut buf = [0u8; 4];
    for _ in 0..size {
        readable.read_exact(&mut buf)?;
        arr.push(f32::from_be_bytes(buf));
    }
    Ok(ShapeMatrix::from_vec(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_face_landmarks_model_parsing() {
        let shaper = Shaper::from_readable(
            include_bytes!("../models/shaper_5_face_landmarks.bin")
                .to_vec()
                .as_slice(),
        )
        .expect("parsing failed");

        assert_eq!(shaper.forests.len(), 15);
        assert_eq!(shaper.forests[0].trees.len(), 500);

        assert_eq!(shaper.forests[0].trees[0].nodes.len(), 15);
        assert_eq!(shaper.forests[0].trees[0].shifts.len(), 16);

        assert_eq!(shaper.forests[0].anchors.len(), 800);
        assert_eq!(shaper.forests[0].deltas.len(), 800);
    }
}
