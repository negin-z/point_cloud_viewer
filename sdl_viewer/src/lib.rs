// Copyright 2016 The Cartographer Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate cgmath;
extern crate clap;
extern crate collision;
extern crate fnv;
extern crate lru_cache;
extern crate point_viewer;
extern crate point_viewer_grpc;
extern crate rand;
extern crate sdl2;
extern crate time;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

/// Unsafe macro to create a static null-terminated c-string for interop with OpenGL.
#[macro_export]
macro_rules! c_str {
    ($s:expr) => {
        concat!($s, "\0").as_ptr() as *const i8
    }
}

mod glhelper;
mod camera;
#[allow(non_upper_case_globals)]
pub mod opengl {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
pub mod box_drawer;
pub mod graphic;
pub mod node_drawer;

use box_drawer::BoxDrawer;
use camera::Camera;
use cgmath::{Matrix4, SquareMatrix};
use fnv::FnvHashMap;
use node_drawer::{NodeDrawer, NodeViewContainer};
use point_viewer::color::YELLOW;
use point_viewer::octree::{self, Octree};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Scancode, LCTRLMOD, RCTRLMOD, LSHIFTMOD, RSHIFTMOD};
use sdl2::video::GLProfile;
use std::cmp;
use std::io;
use std::error::Error;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread;

type OctreeFactory = fn(&String) -> Result<Box<Octree>, Box<Error>>;

pub struct SdlViewer {
    octree_factories: FnvHashMap<String, OctreeFactory>,
}

struct PointCloudRenderer {
    gl: Rc<opengl::Gl>,
    node_drawer: NodeDrawer,
    last_moving: time::PreciseTime,
    // TODO(sirver): Logging does not fit into this classes responsibilities.
    last_log: time::PreciseTime,
    visible_nodes: Vec<octree::NodeId>,
    get_visible_nodes_params_tx: mpsc::Sender<Matrix4<f32>>,
    get_visible_nodes_result_rx: mpsc::Receiver<Vec<octree::NodeId>>,
    num_frames: u32,
    point_size: f32,
    gamma: f32,
    needs_drawing: bool,
    max_nodes_in_memory: usize,
    world_to_gl: Matrix4<f32>,
    max_nodes_moving: usize,
    show_octree_nodes: bool,
    node_views: NodeViewContainer,
    box_drawer: BoxDrawer,
}

#[derive(Debug)]
enum DrawResult {
    HasDrawn,
    NoChange,
}
impl PointCloudRenderer {
    pub fn new(
        max_nodes_in_memory: usize,
        gl: Rc<opengl::Gl>,
        octree: Arc<Box<octree::Octree>>,
    ) -> Self {
        let now = time::PreciseTime::now();

        // This thread waits for requests to calculate the currently visible nodes, runs a
        // calculation and sends the visible nodes back to the drawing thread. If multiple requests
        // queue up while it is processing one, it will drop all but the latest one before
        // restarting the next calculation.
        let (get_visible_nodes_params_tx, rx) = mpsc::channel::<Matrix4<f32>>();
        let (tx, get_visible_nodes_result_rx) = mpsc::channel();
        let octree_clone = octree.clone();
        thread::spawn(move || {
            while let Ok(mut matrix) = rx.recv() {
                // Drain the channel, we only ever want to update the latest.
                while let Ok(newer_matrix) = rx.try_recv() {
                    matrix = newer_matrix;
                }
                let now = ::std::time::Instant::now();
                let visible_nodes = octree_clone.get_visible_nodes(&matrix);
                println!(
                    "Currently visible nodes: {}, time to calculate: {:?}",
                    visible_nodes.len(),
                    now.elapsed()
                );
                tx.send(visible_nodes).unwrap();
            }
        });

        Self {
            last_moving: now,
            last_log: now,
            visible_nodes: Vec::new(),
            node_drawer: NodeDrawer::new(Rc::clone(&gl)),
            num_frames: 0,
            point_size: 1.,
            gamma: 1.,
            get_visible_nodes_params_tx,
            get_visible_nodes_result_rx,
            max_nodes_moving: max_nodes_in_memory,
            needs_drawing: true,
            show_octree_nodes: false,
            max_nodes_in_memory,
            node_views: NodeViewContainer::new(octree, max_nodes_in_memory),
            box_drawer: BoxDrawer::new(Rc::clone(&gl)),
            world_to_gl: Matrix4::identity(),
            gl,
        }
    }

    pub fn camera_changed(&mut self, world_to_gl: &Matrix4<f32>) {
        self.last_moving = time::PreciseTime::now();
        self.needs_drawing = true;
        self.node_drawer.update_world_to_gl(world_to_gl);
        self.get_visible_nodes_params_tx
            .send(world_to_gl.clone())
            .unwrap();
        self.last_moving = time::PreciseTime::now();
        self.world_to_gl = world_to_gl.clone();
    }

    pub fn toggle_show_octree_nodes(&mut self) {
        self.show_octree_nodes = !self.show_octree_nodes;
    }

    pub fn adjust_gamma(&mut self, delta: f32) {
        self.gamma += delta;
        self.needs_drawing = true;
    }

    pub fn adjust_point_size(&mut self, delta: f32) {
        // Point size == 1. is the smallest that is rendered.
        self.point_size = (self.point_size + delta).max(1.);
        self.needs_drawing = true;
    }

    pub fn draw(&mut self) -> DrawResult {
        let mut draw_result = DrawResult::NoChange;
        let mut num_points_drawn = 0;
        let mut num_nodes_drawn = 0;

        let now = time::PreciseTime::now();
        let moving = self.last_moving.to(now) < time::Duration::milliseconds(150);
        self.needs_drawing |= self.node_views
            .consume_arrived_nodes(&self.node_drawer.program);
        while let Ok(visible_nodes) = self.get_visible_nodes_result_rx.try_recv() {
            self.visible_nodes.clear();
            self.visible_nodes.extend(visible_nodes);
            self.needs_drawing = true;
        }

        if self.needs_drawing {
            unsafe {
                self.gl.ClearColor(0., 0., 0., 1.);
                self.gl
                    .Clear(opengl::COLOR_BUFFER_BIT | opengl::DEPTH_BUFFER_BIT);
            }
        }

        // We use a heuristic to keep the frame rate as stable as possible by increasing/decreasing the number of nodes to draw.
        let max_nodes_to_display =
           if moving { self.max_nodes_moving } else { self.max_nodes_in_memory };
        let filtered_visible_nodes = self.visible_nodes.iter().take(max_nodes_to_display);

        for node_id in filtered_visible_nodes {
            let view = self.node_views.get_or_request(&node_id);
            if !self.needs_drawing || view.is_none() {
                continue;
            }
            let view = view.unwrap();
            num_points_drawn += self.node_drawer.draw(
                view,
                1, /* level of detail */
                self.point_size,
                self.gamma,
            );
            num_nodes_drawn += 1;

            if self.show_octree_nodes {
                self.box_drawer
                    .draw_outlines(&view.meta.bounding_cube.to_aabb3(), &self.world_to_gl, &YELLOW);
            }
        }
        if self.needs_drawing {
            draw_result = DrawResult::HasDrawn;
        }
        self.needs_drawing = moving;

        self.num_frames += 1;
        let now = time::PreciseTime::now();
        if self.last_log.to(now) > time::Duration::seconds(1) {
            let duration = self.last_log.to(now).num_microseconds().unwrap();
            let fps = (self.num_frames * 1_000_000u32) as f32 / duration as f32;
            if moving {
                if fps < 20. {
                    self.max_nodes_moving = (self.max_nodes_moving as f32 * 0.9) as usize;
                }
                if fps > 25. && self.max_nodes_moving < self.max_nodes_in_memory {
                    self.max_nodes_moving = (self.max_nodes_moving as f32 * 1.1) as usize;
                }
            }
            self.num_frames = 0;
            self.last_log = now;
            println!(
                "FPS: {:#?}, Drew {} points from {} loaded nodes. {} nodes \
                 should be shown, Cache {} MB",
                fps,
                num_points_drawn,
                num_nodes_drawn,
                self.visible_nodes.len(),
                self.node_views.get_used_memory_bytes() as f32 / 1024. / 1024.,
            );
        }
        draw_result
    }
}

#[derive(Debug,Serialize,Deserialize)]
pub struct CameraStates {
    states: Vec<camera::State>,
}

fn save_camera(index: usize, pose_path: &Option<PathBuf>, camera: &Camera) {
    if pose_path.is_none() {
        println!("Not serving from a local directory. Cannot save camera.");
        return;
    }
    assert!(index < 10);
    let mut states = ::std::fs::read_to_string(pose_path.as_ref().unwrap()).and_then(|data| {
        serde_json::from_str(&data).map_err(|_| io::Error::new(io::ErrorKind::Other, "Could not read camera file."))
    }).unwrap_or_else(|_| CameraStates{ states: vec![camera.state(); 10] });
    states.states[index] = camera.state();

    match std::fs::write(pose_path.as_ref().unwrap(), serde_json::to_string_pretty(&states).unwrap().as_bytes()) {
        Ok(_) => (),
        Err(e) => println!("Could not write {}: {}", pose_path.as_ref().unwrap().display(), e),
    }
    println!("Saved current camera position as {}.", index);
}

fn load_camera(index: usize, pose_path: &Option<PathBuf>, camera: &mut Camera) {
    if pose_path.is_none() {
        println!("Not serving from a local directory. Cannot load camera.");
        return;
    }
    assert!(index < 10);
    let states = ::std::fs::read_to_string(pose_path.as_ref().unwrap()).and_then(|data| {
        serde_json::from_str(&data).map_err(|_| io::Error::new(io::ErrorKind::Other, "Could not read camera file."))
    }).unwrap_or_else(|_| CameraStates{ states: vec![camera.state(); 10] });
    camera.set_state(states.states[index]);
}

impl SdlViewer {
    pub fn new() -> Self {
        SdlViewer {
            octree_factories: FnvHashMap::default(),
        }
    }

    // Registers a callback 'function' that is called whenever the octree commandline argument
    // starts with its 'prefix'
    // The callback function creates and returns an Octree
    pub fn register_octree_factory(mut self, prefix: String, function: OctreeFactory) -> SdlViewer {
        self.octree_factories.insert(prefix, function);
        self
    }

    pub fn run(self) {
        let matches = clap::App::new("sdl_viewer")
            .args(&[
                clap::Arg::with_name("octree")
                    .help("Input path of the octree.")
                    .index(1)
                    .required(true),
                clap::Arg::with_name("cache_size_mb")
                    .help(
                        "Maximum cache size in MB for octree nodes in GPU memory. \
                         The default value is 2000 MB and the valid range is 1000 MB to 16000 MB.",
                    )
                    .required(false),
            ])
            .get_matches();

        let octree_argument = matches.value_of("octree").unwrap();

        // Maximum number of MB for the octree node cache. The default is 2 GB
        let cache_size_mb: usize = matches
            .value_of("cache_size_mb")
            .unwrap_or("2000")
            .parse()
            .expect("Could not parse 'cache_size_mb' option.");

        // Maximum number of MB for the octree node cache in range 1..16 GB. The default is 2 GB
        let limit_cache_size_mb = cmp::max(1000, cmp::min(16_000, cache_size_mb));

        // Assuming about 200 KB per octree node on average
        let max_nodes_in_memory = limit_cache_size_mb * 5;

        // call octree generation functions
        let mut octree_opt: Option<Box<Octree>> = None;
        let mut pose_path = None;
        for (prefix, octree_factory_function) in &self.octree_factories {
            if !octree_argument.starts_with(prefix) {
                continue;
            }
            let no_prefix = &octree_argument[prefix.len()..].to_string();
            if let Ok(o) = octree_factory_function(no_prefix) {
                octree_opt = Some(o);
                break;
            }
        }

        // If no octree was generated create an FromDisc loader
        let octree = Arc::new(octree_opt.unwrap_or_else(|| {
            pose_path = Some(PathBuf::from(&octree_argument).join("poses.json"));
            Box::new(octree::OnDiskOctree::new(&octree_argument).unwrap()) as Box<Octree>
        }));

        let ctx = sdl2::init().unwrap();
        let video_subsystem = ctx.video().unwrap();

        // We need to open the joysticks we are interested in and keep the object alive to receive
        // input from it. We just open the first we find.
        let joystick_subsystem = ctx.joystick().unwrap();
        let joystick = match joystick_subsystem.open(0) {
            Ok(j) => {
                println!("Found a joystick and will use it.");
                Some(j)
            },
            Err(_) => None,
        };

        let gl_attr = video_subsystem.gl_attr();

        // TODO(hrapp): This should use OpenGL ES 2.0 to be compatible with WebGL, so this can be made
        // to work with emscripten.
        gl_attr.set_context_profile(GLProfile::Core);
        gl_attr.set_context_version(3, 2);

        const WINDOW_WIDTH: i32 = 800;
        const WINDOW_HEIGHT: i32 = 600;
        let window = match video_subsystem
            .window("sdl2_viewer", WINDOW_WIDTH as u32, WINDOW_HEIGHT as u32)
            .position_centered()
            .resizable()
            .opengl()
            .build()
        {
            Ok(window) => window,
            Err(err) => panic!("failed to create window: {}", err),
        };

        // We need to create a context now, only after can we actually legally load the gl functions
        // and query 'gl_attr'.
        let _context = window.gl_create_context().unwrap();
        video_subsystem.gl_set_swap_interval(1);

        assert_eq!(gl_attr.context_profile(), GLProfile::Core);

        let gl = Rc::new(opengl::Gl::load_with(|s| {
            let ptr = video_subsystem.gl_get_proc_address(s);
            unsafe { std::mem::transmute(ptr) }
        }));

        let mut renderer = PointCloudRenderer::new(max_nodes_in_memory, Rc::clone(&gl), octree);
        let mut camera = Camera::new(&gl, WINDOW_WIDTH, WINDOW_HEIGHT);

        let mut events = ctx.event_pump().unwrap();
        'outer_loop: loop {

            for event in events.poll_iter() {
                match event {
                    Event::Quit { .. } => break 'outer_loop,
                    Event::KeyDown {
                        scancode: Some(code),
                        keymod,
                        ..
                    } => {
                        if keymod.is_empty() {
                            match code {
                                Scancode::Escape => break 'outer_loop,
                                Scancode::W => camera.moving_forward = true,
                                Scancode::S => camera.moving_backward = true,
                                Scancode::A => camera.moving_left = true,
                                Scancode::D => camera.moving_right = true,
                                Scancode::Z => camera.moving_down = true,
                                Scancode::Q => camera.moving_up = true,
                                Scancode::Left => camera.turning_left = true,
                                Scancode::Right => camera.turning_right = true,
                                Scancode::Down => camera.turning_down = true,
                                Scancode::Up => camera.turning_up = true,
                                Scancode::O => renderer.toggle_show_octree_nodes(),
                                Scancode::Num7 => renderer.adjust_gamma(-0.1),
                                Scancode::Num8 => renderer.adjust_gamma(0.1),
                                Scancode::Num9 => renderer.adjust_point_size(-0.1),
                                Scancode::Num0 => renderer.adjust_point_size(0.1),
                                _ => (),
                            }
                        } else if keymod.intersects(LCTRLMOD | RCTRLMOD) && keymod.intersects(LSHIFTMOD | RSHIFTMOD) {
                            // CTRL + SHIFT is pressed.
                            match code {
                                Scancode::Num1 => save_camera(0, &pose_path, &camera),
                                Scancode::Num2 => save_camera(1, &pose_path, &camera),
                                Scancode::Num3 => save_camera(2, &pose_path, &camera),
                                Scancode::Num4 => save_camera(3, &pose_path, &camera),
                                Scancode::Num5 => save_camera(4, &pose_path, &camera),
                                Scancode::Num6 => save_camera(5, &pose_path, &camera),
                                Scancode::Num7 => save_camera(6, &pose_path, &camera),
                                Scancode::Num8 => save_camera(7, &pose_path, &camera),
                                Scancode::Num9 => save_camera(8, &pose_path, &camera),
                                Scancode::Num0 => save_camera(9, &pose_path, &camera),
                                _ => (),
                            }
                        } else if keymod.intersects(LCTRLMOD | RCTRLMOD) {
                            // CTRL is pressed.
                            match code {
                                Scancode::Num1 => load_camera(0, &pose_path, &mut camera),
                                Scancode::Num2 => load_camera(1, &pose_path, &mut camera),
                                Scancode::Num3 => load_camera(2, &pose_path, &mut camera),
                                Scancode::Num4 => load_camera(3, &pose_path, &mut camera),
                                Scancode::Num5 => load_camera(4, &pose_path, &mut camera),
                                Scancode::Num6 => load_camera(5, &pose_path, &mut camera),
                                Scancode::Num7 => load_camera(6, &pose_path, &mut camera),
                                Scancode::Num8 => load_camera(7, &pose_path, &mut camera),
                                Scancode::Num9 => load_camera(8, &pose_path, &mut camera),
                                Scancode::Num0 => load_camera(9, &pose_path, &mut camera),
                                _ => (),
                            }
                        }
                    },
                    Event::KeyUp {
                        scancode: Some(code),
                        ..
                    } => match code {
                        Scancode::W => camera.moving_forward = false,
                        Scancode::S => camera.moving_backward = false,
                        Scancode::A => camera.moving_left = false,
                        Scancode::D => camera.moving_right = false,
                        Scancode::Z => camera.moving_down = false,
                        Scancode::Q => camera.moving_up = false,
                        Scancode::Left => camera.turning_left = false,
                        Scancode::Right => camera.turning_right = false,
                        Scancode::Down => camera.turning_down = false,
                        Scancode::Up => camera.turning_up = false,
                        _ => (),
                    },
                    Event::MouseMotion {
                        xrel,
                        yrel,
                        mousestate,
                        ..
                    } => {
                        if mousestate.left() {
                            camera.mouse_drag_rotate(xrel, yrel)
                        } else if mousestate.right() {
                            camera.mouse_drag_pan(xrel, yrel)
                        }
                    }
                    Event::MouseWheel { y, .. } => {
                        camera.mouse_wheel(y);
                    }
                    Event::Window {
                        win_event: WindowEvent::SizeChanged(w, h),
                        ..
                    } => {
                        camera.set_size(&gl, w, h);
                    }
                    _ => (),
                }
            }

            if let Some(j) = joystick.as_ref() {
                let x = j.axis(0).unwrap() as f32 / 1000.;
                let y = -j.axis(1).unwrap() as f32 / 1000.;
                let z = -j.axis(2).unwrap() as f32 / 1000.;
                let up = j.axis(3).unwrap() as f32 / 10000.;
                // Combine tilting and turning on the knob.
                let around = j.axis(4).unwrap() as f32 / 10000. - j.axis(5).unwrap() as f32 / 10000.;
                camera.pan(x, y, z);
                camera.rotate(up, around);
            }

            if camera.update() {
                renderer.camera_changed(&camera.get_world_to_gl());
            }

            match renderer.draw() {
                DrawResult::HasDrawn => window.gl_swap_window(),
                DrawResult::NoChange => (),
            }
        }
    }
}
