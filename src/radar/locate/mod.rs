use std::collections::{HashMap, VecDeque};

use anyhow::{anyhow, Result};
use image::{ImageBuffer, Luma};
use nalgebra::{Const, Matrix3, Matrix4, OMatrix, Point3, Vector3, Vector4};
use rayon::prelude::*;

use super::detect::{BBox, RobotDetection};
use cluster::dbscan;

mod cluster;

const DEPTH_MAP_QUEUE_SIZE: usize = 3;

struct Transform {
    transform_matrix: Matrix4<f32>,
    transform_matrix_inverse: Matrix4<f32>,
    rotation_matrix: Matrix3<f32>,
    rotation_matrix_inverse: Matrix3<f32>,
    translation_vector: Vector3<f32>,
    translation_vector_inverse: Vector3<f32>,
}

struct MatrixWithInverse<const DIM: usize> {
    matrix: OMatrix<f32, Const<DIM>, Const<DIM>>,
    matrix_inverse: OMatrix<f32, Const<DIM>, Const<DIM>>,
}

pub struct RobotLocation {
    pub center: Point3<f32>,
    pub width: f32,
    pub height: f32,
    pub depth: f32,
}

impl TryFrom<Matrix4<f32>> for Transform {
    type Error = anyhow::Error;

    fn try_from(transform_matrix: Matrix4<f32>) -> std::result::Result<Self, Self::Error> {
        let transform_matrix_inverse = transform_matrix
            .try_inverse()
            .ok_or_else(|| anyhow!("Failed to invert transform matrix {:#?}", transform_matrix))?;
        let rotation_matrix: Matrix3<f32> = transform_matrix.fixed_view::<3, 3>(0, 0).into();
        let rotation_matrix_inverse = rotation_matrix
            .try_inverse()
            .ok_or_else(|| anyhow!("Failed to invert rotation matrix {:#?}", rotation_matrix))?;
        let translation_vector = Vector3::new(
            transform_matrix[(0, 3)],
            transform_matrix[(1, 3)],
            transform_matrix[(2, 3)],
        );
        let translation_vector_inverse = -translation_vector;

        let transform = Transform {
            transform_matrix,
            transform_matrix_inverse,
            rotation_matrix,
            rotation_matrix_inverse,
            translation_vector,
            translation_vector_inverse,
        };

        Ok(transform)
    }
}

impl<const DIM: usize> TryFrom<OMatrix<f32, Const<DIM>, Const<DIM>>> for MatrixWithInverse<DIM> {
    type Error = anyhow::Error;

    fn try_from(
        matrix: OMatrix<f32, Const<DIM>, Const<DIM>>,
    ) -> std::result::Result<Self, Self::Error> {
        let matrix_inverse = matrix
            .try_inverse()
            .ok_or_else(|| anyhow!("Failed to invert matrix {:#?}", matrix))?;

        let matrix_with_inverse = Self {
            matrix,
            matrix_inverse,
        };
        Ok(matrix_with_inverse)
    }
}

pub struct Locator {
    camera_intrinsic: MatrixWithInverse<3>,
    lidar_to_camera: Transform,
    world_to_camera: Transform,
    cluster_epsilon: f32,
    cluster_min_points: usize,
    min_distance_to_background: f32,
    max_distance_to_background: f32,
    max_valid_distance: f32,
    background_depth_map: ImageBuffer<Luma<f32>, Vec<f32>>,
    depth_map_queue: VecDeque<ImageBuffer<Luma<f32>, Vec<f32>>>,
}

impl Locator {
    pub fn new(
        image_width: u32,
        image_height: u32,
        camera_intrinsic: Matrix3<f32>,
        lidar_to_camera_transform: Matrix4<f32>,
        world_to_camera_transform: Matrix4<f32>,
        cluster_epsilon: f32,
        cluster_min_points: usize,
        min_distance_to_background: f32,
        max_distance_to_background: f32,
        max_valid_distance: f32,
    ) -> Result<Self> {
        let locator = Self {
            camera_intrinsic: MatrixWithInverse::try_from(camera_intrinsic)?,
            lidar_to_camera: Transform::try_from(lidar_to_camera_transform)?,
            world_to_camera: Transform::try_from(world_to_camera_transform)?,
            cluster_epsilon,
            cluster_min_points,
            min_distance_to_background,
            max_distance_to_background,
            max_valid_distance,
            background_depth_map: ImageBuffer::new(image_width, image_height),
            depth_map_queue: VecDeque::with_capacity(DEPTH_MAP_QUEUE_SIZE),
        };

        Ok(locator)
    }

    pub fn locate_detections(
        &mut self,
        points: &[Point3<f32>],
        detections: &[RobotDetection],
    ) -> Vec<Option<RobotLocation>> {
        let robot_depth_map = self.get_robot_depth_map(points);

        let pixels_category_mapping = self.cluster_and_get_category(&robot_depth_map);

        let bboxes: Vec<_> = detections.iter().map(|det| det.bbox()).collect();

        self.search_for_location(&bboxes, robot_depth_map, pixels_category_mapping)
    }

    fn lidar_to_world(&self, point: &Point3<f32>) -> Point3<f32> {
        let lidar_coor_vector = Vector4::new(point.x, point.y, point.z, 1.0);

        let camera_to_world_transform = &self.world_to_camera.transform_matrix_inverse;
        let lidar_to_camera_transform = &self.lidar_to_camera.transform_matrix;
        let world_coor_vector =
            camera_to_world_transform * lidar_to_camera_transform * lidar_coor_vector;

        Point3::new(
            world_coor_vector[0],
            world_coor_vector[1],
            world_coor_vector[2],
        )
    }

    fn camera_to_lidar(&self, point: &Point3<f32>) -> Point3<f32> {
        let camera_coor_vector = Vector3::new(point.x, point.y, 1.0);

        let camera_to_lidar_rotate = &self.lidar_to_camera.rotation_matrix_inverse;
        let camera_intrinsic_inverse = &self.camera_intrinsic.matrix_inverse;
        let camera_to_lidar_translate = &self.lidar_to_camera.translation_vector_inverse;
        let lidar_coor_vector = camera_to_lidar_rotate
            * (camera_intrinsic_inverse * point.z * camera_coor_vector + camera_to_lidar_translate);
        Point3::new(
            lidar_coor_vector[0],
            lidar_coor_vector[1],
            lidar_coor_vector[2],
        )
    }

    fn lidar_to_camera(&self, point: &Point3<f32>) -> Point3<f32> {
        let lidar_coor_vector = Vector4::new(point.x, point.y, point.z, 1.0);

        let lidar_to_camera_transform = &self.lidar_to_camera.transform_matrix;
        let camera_coor_vector = self.camera_intrinsic.matrix
            * (lidar_to_camera_transform * lidar_coor_vector).view((0, 0), (3, 1));
        Point3::new(
            camera_coor_vector[0] / camera_coor_vector[2],
            camera_coor_vector[1] / camera_coor_vector[2],
            camera_coor_vector[2],
        )
    }

    fn get_robot_depth_map(&mut self, points: &[Point3<f32>]) -> ImageBuffer<Luma<f32>, Vec<f32>> {
        let (image_width, image_height) = self.background_depth_map.dimensions();
        let mut depth_map: ImageBuffer<Luma<f32>, Vec<_>> =
            ImageBuffer::new(image_width, image_height);

        for point in points {
            if point.is_empty()
                || !point.x.is_normal()
                || !point.y.is_normal()
                || !point.z.is_normal()
            {
                continue;
            }

            if point.x > self.max_valid_distance {
                continue;
            }

            let camera_point = self.lidar_to_camera(point);
            let (u, v, depth) = (
                camera_point.x.round() as i32,
                camera_point.y.round() as i32,
                camera_point.z as f32,
            );
            if u < 0 || u as u32 >= image_width || v < 0 || v as u32 >= image_height {
                continue;
            }
            depth_map.put_pixel(u as u32, v as u32, Luma([depth]));

            let background_depth = self.background_depth_map.get_pixel_mut(u as u32, v as u32);
            if depth > background_depth.0[0] {
                background_depth.0[0] = depth;
            }
        }

        self.depth_map_queue.push_back(depth_map);
        if self.depth_map_queue.len() > DEPTH_MAP_QUEUE_SIZE {
            self.depth_map_queue.pop_front();
        }

        let mut difference_depth_map: ImageBuffer<Luma<f32>, Vec<f32>> =
            ImageBuffer::new(image_width, image_height);
        self.depth_map_queue.iter().for_each(|depth_map| {
            difference_depth_map
                .enumerate_pixels_mut()
                .par_bridge()
                .for_each(|(x, y, pixel)| {
                    let depth_value = depth_map.get_pixel(x, y).0[0];
                    let background_depth_value = self.background_depth_map.get_pixel(x, y).0[0];

                    let difference = (depth_value - background_depth_value).abs();
                    if difference > self.min_distance_to_background
                        && difference < self.max_distance_to_background
                    {
                        pixel.0[0] = difference;
                    }
                });
        });

        difference_depth_map
    }

    fn cluster_and_get_category(
        &self,
        difference_depth_map: &ImageBuffer<Luma<f32>, Vec<f32>>,
    ) -> HashMap<(u32, u32), isize> {
        let camera_points: Vec<_> = difference_depth_map
            .enumerate_pixels()
            .par_bridge()
            .filter_map(|(x, y, pixel)| {
                let depth = pixel.0[0];
                if depth.is_normal() {
                    Some((x, y, depth))
                } else {
                    None
                }
            })
            .collect();

        let lidar_points: Vec<_> = camera_points
            .iter()
            .map(|(x, y, depth)| self.camera_to_lidar(&Point3::new(*x as f32, *y as f32, *depth)))
            .collect();

        let categories = dbscan(&lidar_points, self.cluster_epsilon, self.cluster_min_points);

        let mut mapping = HashMap::with_capacity(categories.len());
        camera_points
            .into_iter()
            .zip(categories.into_iter())
            .for_each(|((pixel_x, pixel_y, _depth), category)| {
                mapping.insert((pixel_x, pixel_y), category);
            });

        mapping
    }

    fn search_for_location(
        &self,
        bboxes: &[BBox],
        difference_depth_map: ImageBuffer<Luma<f32>, Vec<f32>>,
        cluster_result: HashMap<(u32, u32), isize>,
    ) -> Vec<Option<RobotLocation>> {
        let (image_width, image_height) = difference_depth_map.dimensions();

        bboxes
            .iter()
            .map(|bbox| {
                let mut category_pixels: HashMap<isize, Vec<(u32, u32)>> = HashMap::new();
                let (x_min, x_max, y_min, y_max) = (
                    (bbox.x_center - bbox.width / 2.0).max(0.0).floor() as u32,
                    (bbox.x_center + bbox.width / 2.0).ceil() as u32,
                    (bbox.y_center - bbox.height / 2.0).max(0.0).floor() as u32,
                    (bbox.y_center + bbox.height / 2.0).ceil() as u32,
                );

                for y in y_min..=y_max {
                    if y >= image_height {
                        break;
                    }
                    for x in x_min..=x_max {
                        if x >= image_width {
                            break;
                        }
                        if let Some(&category) = cluster_result.get(&(x, y)) {
                            category_pixels
                                .entry(category)
                                .or_insert_with(Vec::new)
                                .push((x, y));
                        }
                    }
                }

                if let Some((_, pixels)) = category_pixels
                    .iter()
                    .max_by_key(|&(_, pixels)| pixels.len())
                {
                    let (sum_point, count, min_max) = pixels
                        .iter()
                        .filter_map(|&(x, y)| {
                            let depth = difference_depth_map.get_pixel(x, y).0[0];
                            if depth.is_normal() {
                                Some(self.camera_to_lidar(&Point3::new(x as f32, y as f32, depth)))
                            } else {
                                None
                            }
                        })
                        .fold(
                            (
                                Point3::<f32>::new(0.0, 0.0, 0.0),
                                0,
                                (
                                    Point3::<f32>::new(f32::MAX, f32::MAX, f32::MAX),
                                    Point3::<f32>::new(f32::MIN, f32::MIN, f32::MIN),
                                ),
                            ),
                            |(sum, cnt, (min_point, max_point)), point| {
                                (
                                    Point3::new(sum.x + point.x, sum.y + point.y, sum.z + point.z),
                                    cnt + 1,
                                    (
                                        Point3::new(
                                            min_point.x.min(point.x),
                                            min_point.y.min(point.y),
                                            min_point.z.min(point.z),
                                        ),
                                        Point3::new(
                                            max_point.x.max(point.x),
                                            max_point.y.max(point.y),
                                            max_point.z.max(point.z),
                                        ),
                                    ),
                                )
                            },
                        );

                    if count > 0 {
                        let robot_location = RobotLocation {
                            center: Point3::new(
                                sum_point.x / count as f32,
                                sum_point.y / count as f32,
                                sum_point.z / count as f32,
                            ),
                            width: min_max.1.x - min_max.0.x,
                            height: min_max.1.y - min_max.0.y,
                            depth: min_max.1.z - min_max.0.z,
                        };

                        Some(robot_location)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;
    use nalgebra::{Matrix3, Matrix4, Point3};

    #[test]
    fn test_transform_inverse() {
        #[rustfmt::skip]
        let transform_matrix = Matrix4::new(
            1.0, 0.0, 0.0, 2.0,  
            0.0, 1.0, 0.0, 3.0,
            0.0, 0.0, 1.0, 4.0,
            0.0, 0.0, 0.0, 1.0,
        );

        let transform = Transform::try_from(transform_matrix).unwrap();

        let expected_inverse = transform_matrix.try_inverse().unwrap();
        assert_eq!(transform.transform_matrix_inverse, expected_inverse);

        #[rustfmt::skip]
        let expected_rotation = Matrix3::new(
            1.0, 0.0, 0.0,
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
        );

        assert_eq!(transform.rotation_matrix, expected_rotation);
        assert_eq!(transform.translation_vector, Vector3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn test_matrix_with_inverse() {
        #[rustfmt::skip]
        let matrix = Matrix3::new(
            1.0, 2.0, 3.0,
            0.0, 1.0, 4.0,
            5.0, 6.0, 0.0,
        );

        let matrix_with_inverse = MatrixWithInverse::try_from(matrix).unwrap();

        let expected_inverse = matrix.try_inverse().unwrap();
        assert_eq!(matrix_with_inverse.matrix_inverse, expected_inverse);
    }

    #[test]
    fn test_lidar_camera_conversion() {
        #[rustfmt::skip]
        let camera_intrinsic = Matrix3::new(
            1.0, 0.0, 0.0,
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
        );
        let lidar_to_camera_transform = Matrix4::identity();
        let world_to_camera_transform = Matrix4::identity();

        let locator = Locator::new(
            640,
            480,
            camera_intrinsic,
            lidar_to_camera_transform,
            world_to_camera_transform,
            0.5,
            10,
            0.1,
            10.0,
            100.0,
        )
        .unwrap();

        let lidar_point = Point3::new(1.0, 2.0, 3.0);
        let camera_point = locator.lidar_to_camera(&lidar_point);
        let converted_back = locator.camera_to_lidar(&camera_point);

        assert_approx_eq!((lidar_point - converted_back).norm(), 0.0);
    }

    #[test]
    fn test_get_robot_depth_map() {
        #[rustfmt::skip]
        let camera_intrinsic = Matrix3::new(
            1.0, 0.0, 0.0,
            0.0, 1.0, 0.0,
            0.0, 0.0, 1.0,
        );
        let lidar_to_camera_transform = Matrix4::identity();
        let world_to_camera_transform = Matrix4::identity();

        let mut locator = Locator::new(
            640,
            480,
            camera_intrinsic,
            lidar_to_camera_transform,
            world_to_camera_transform,
            0.5,
            10,
            0.1,
            10.0,
            100.0,
        )
        .unwrap();

        let points_0 = vec![Point3::new(2.0, 3.0, 1.0)];
        let points_1 = vec![Point3::new(1.0, 2.0, 3.0), Point3::new(2.0, 3.0, 1.0)];

        locator.get_robot_depth_map(&points_0);
        let depth_map = locator.get_robot_depth_map(&points_1);

        let pixel = depth_map.get_pixel(0, 1);
        assert_approx_eq!(pixel.0[0], 3.0);
    }
}
