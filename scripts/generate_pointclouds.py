import rospy
import rosbag
import numpy as np
from sensor_msgs.msg import PointCloud2, PointField
import struct
import time


# Function to write random pointcloud data to a bag file
def write_random_pointcloud_bag(bag_file, topic_name, num_frames):
    try:
        rospy.init_node('vidbag2mmd', anonymous=True)
        print('Ros node initialized.')
    except rospy.exceptions.ROSException:
        print('Node has already been initialized.')

    print(f"Creating bag file: {bag_file}")

    with rosbag.Bag(bag_file, 'w') as bag:
        for i in range(num_frames):
            print(f"Generating frame {i+1}/{num_frames}")

            # Generate a new frame of point cloud data
            num_points = 10
            print(f"Generating point cloud with {num_points} points.")

            fields = [
                PointField('x', 0, PointField.FLOAT32, 1),
                PointField('y', 4, PointField.FLOAT32, 1),
                PointField('z', 8, PointField.FLOAT32, 1),
                PointField('intensity', 12, PointField.FLOAT32, 1)
            ]

            # Generate random points for x, y, z and intensity
            # 100 points, 4 values each (x, y, z, intensity)
            points = np.random.rand(num_points, 4).astype(np.float32)

            # Pack the points into binary format
            cloud_data = []
            for p in points:
                cloud_data.append(struct.pack('ffff', p[0], p[1], p[2], p[3]))

            # Create PointCloud2 message
            point_cloud_msg = PointCloud2()
            point_cloud_msg.header.stamp = rospy.Time.now()
            point_cloud_msg.header.frame_id = "velodyne"
            point_cloud_msg.height = 1  # unstructured data
            point_cloud_msg.width = num_points
            point_cloud_msg.fields = fields
            point_cloud_msg.is_bigendian = False
            point_cloud_msg.point_step = 16  # 4 floats * 4 bytes
            point_cloud_msg.row_step = point_cloud_msg.point_step * num_points
            point_cloud_msg.is_dense = True  # No invalid points
            point_cloud_msg.data = b''.join(cloud_data)

            # Write the point cloud message to the bag under the specified topic
            print(
                f"Writing frame {i+1}/{num_frames} to topic {topic_name}")
            bag.write(topic_name, point_cloud_msg, rospy.Time.now())

            # Sleep briefly between frames to simulate time progression
            time.sleep(0.1)  # 100ms delay between frames

    print(f"Finished writing {num_frames} frames to {bag_file}")