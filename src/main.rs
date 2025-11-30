use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;
use serde::{Serialize, Deserialize};

// File format: 9-byte header + pixel data
// Header: [mode: u8, width: u32 (LE), height: u32 (LE)]
const HEADER_SIZE: u64 = 9;
use rayon::prelude::*;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey, ModifiersState};
use winit::window::{Window, WindowId};
use pixels::{Pixels, SurfaceTexture};
use image::GenericImageView;

/// Represents a point on the board
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Board mode - blackboard (dark) or whiteboard (light)
#[derive(Debug, Clone, Copy, PartialEq)]
enum BoardMode {
    Blackboard,
    Whiteboard,
}

impl BoardMode {
    fn background_color(&self) -> [u8; 4] {
        match self {
            BoardMode::Blackboard => [15, 15, 15, 255],  // Dark grey
            BoardMode::Whiteboard => [255, 255, 255, 255], // Pure white
        }
    }

    fn default_pen_color(&self) -> [u8; 4] {
        match self {
            BoardMode::Blackboard => [255, 255, 255, 255], // White chalk
            BoardMode::Whiteboard => [0, 0, 0, 255],    // Black marker (inverts perfectly with white)
        }
    }
}

/// Represents the board configuration
#[derive(Debug)]
struct BoardConfig {
    width: u32,
    height: u32,
    pixel_size: usize,
    mode: BoardMode,
}

/// Main board structure with cylindrical topology
struct Board {
    config: BoardConfig,
    data_file: File,
    pub viewport: Viewport,
    cache: Vec<u8>,  // In-memory cache of entire board for fast rendering (background only)
    drawing_layer: Vec<u8>,  // Transparent drawing layer on top of posters (RGBA)
    undo_stack: Vec<Vec<u8>>,  // Store up to 3 previous drawing layer states
    has_drawings: bool,  // Track if drawing layer has any non-transparent pixels
    // Viewport render cache
    viewport_cache: Vec<u8>,  // Cached rendered viewport
    cached_viewport_width: u32,
    cached_viewport_height: u32,
    cached_viewport_pos: Point,
    cached_viewport_zoom: f32,
    viewport_dirty: bool,
}

/// Camera/viewport for navigation
pub struct Viewport {
    pub position: Point,
    pub zoom: f32,
}

impl Board {
    /// Create a new board with specified dimensions
    fn new(width: u32, height: u32, mode: BoardMode, file_path: &Path) -> io::Result<Self> {
        let file_exists = file_path.exists();
        
        // Check if existing file has valid header
        let has_valid_header = if file_exists {
            if let Ok(metadata) = std::fs::metadata(file_path) {
                metadata.len() > HEADER_SIZE
            } else {
                false
            }
        } else {
            false
        };
        
        let mut data_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(file_path)?;

        let (loaded_mode, loaded_width, loaded_height) = if has_valid_header {
            // Read header to get saved mode and dimensions
            let mut header = [0u8; HEADER_SIZE as usize];
            if let Ok(_) = data_file.read_exact(&mut header) {
                let saved_mode = match header[0] {
                    0 => BoardMode::Blackboard,
                    1 => BoardMode::Whiteboard,
                    _ => mode,
                };
                let saved_width = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
                let saved_height = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
                
                // Validate dimensions
                if saved_width > 0 && saved_height > 0 && saved_width <= 100000 && saved_height <= 100000 {
                    println!("Loading existing board: {}x{} ({:?} mode)", saved_width, saved_height, saved_mode);
                    (saved_mode, saved_width, saved_height)
                } else {
                    // Invalid dimensions, use defaults
                    println!("Invalid saved dimensions, creating new board");
                    (mode, width, height)
                }
            } else {
                // Can't read header, use defaults
                println!("Cannot read header, creating new board");
                (mode, width, height)
            }
        } else {
            // No valid header, create new board
            if file_exists {
                println!("Old format detected, creating new board (old data will be overwritten)");
            }
            (mode, width, height)
        };

        let config = BoardConfig {
            width: loaded_width,
            height: loaded_height,
            pixel_size: 4, // RGBA
            mode: loaded_mode,
        };

        // Pre-allocate disk space
        let total_size = HEADER_SIZE + (loaded_width as u64) * (loaded_height as u64) * (config.pixel_size as u64);
        data_file.set_len(total_size)?;

        // Allocate memory cache for entire board
        let cache_size = (loaded_width as usize) * (loaded_height as usize) * 4;
        let cache = vec![0u8; cache_size];
        
        // Allocate transparent drawing layer (all pixels start fully transparent)
        let drawing_layer = vec![0u8; cache_size];
        
        let mut board = Board {
            config,
            data_file,
            viewport: Viewport {
                position: Point { x: 0.0, y: 0.0 },
                zoom: 1.0,
            },
            cache,
            drawing_layer,
            undo_stack: Vec::new(),
            has_drawings: false,  // Will be set to true when loading or drawing
            viewport_cache: Vec::new(),
            cached_viewport_width: 0,
            cached_viewport_height: 0,
            cached_viewport_pos: Point { x: 0.0, y: 0.0 },
            cached_viewport_zoom: 1.0,
            viewport_dirty: true,
        };

        if has_valid_header {
            // Load existing data from disk
            board.load_cache()?;
        } else {
            // Initialize new board with background color and write header
            board.clear()?;
            board.write_header()?;
        }

        Ok(board)
    }
    
    /// Write header with mode and dimensions
    fn write_header(&mut self) -> io::Result<()> {
        let mut header = [0u8; HEADER_SIZE as usize];
        header[0] = match self.config.mode {
            BoardMode::Blackboard => 0,
            BoardMode::Whiteboard => 1,
        };
        header[1..5].copy_from_slice(&self.config.width.to_le_bytes());
        header[5..9].copy_from_slice(&self.config.height.to_le_bytes());
        
        self.data_file.seek(SeekFrom::Start(0))?;
        self.data_file.write_all(&header)?;
        Ok(())
    }
    
    /// Load entire board from disk into memory cache
    fn load_cache(&mut self) -> io::Result<()> {
        self.data_file.seek(SeekFrom::Start(HEADER_SIZE))?;
        self.data_file.read_exact(&mut self.cache)?;
        
        // Load drawing layer if it exists
        if Path::new("drawing_layer.data").exists() {
            let drawing_data = std::fs::read("drawing_layer.data")?;
            if drawing_data.len() == self.drawing_layer.len() {
                self.drawing_layer.copy_from_slice(&drawing_data);
                
                // Check if there are any non-transparent pixels
                self.has_drawings = self.drawing_layer.chunks(4).any(|pixel| pixel[3] != 0);
            }
        }
        
        Ok(())
    }

    /// Draw a pixel at the given position (writes to drawing layer)
    #[inline(always)]
    fn draw_pixel(&mut self, x: i32, y: i32, color: [u8; 4]) {
        // Only wrap horizontally (cylindrical), reject out-of-bounds vertical coords
        if y < 0 || y >= self.config.height as i32 {
            return; // Don't draw outside vertical bounds
        }
        
        let wrapped_x = x.rem_euclid(self.config.width as i32) as u32;
        let y = y as u32;

        let offset = (((y as u64) * (self.config.width as u64) + (wrapped_x as u64)) 
            * (self.config.pixel_size as u64)) as usize;

        // Write to drawing layer using direct pointer write for maximum speed
        unsafe {
            let ptr = self.drawing_layer.as_mut_ptr().add(offset) as *mut u32;
            *ptr = u32::from_ne_bytes(color);
        }
        
        // Mark that we have drawings (if not erasing)
        if color[3] != 0 {
            self.has_drawings = true;
        }
    }
    
    /// Save current drawing layer state to undo stack (keep max 3 states)
    fn save_undo_state(&mut self) {
        let snapshot = self.drawing_layer.clone();
        self.undo_stack.push(snapshot);
        
        // Keep only last 3 states
        if self.undo_stack.len() > 3 {
            self.undo_stack.remove(0);
        }
    }
    
    /// Undo last operation by restoring previous drawing layer state
    fn undo(&mut self) -> bool {
        if let Some(previous_state) = self.undo_stack.pop() {
            self.drawing_layer = previous_state;
            true
        } else {
            false
        }
    }
    
    /// Sync pending changes to disk (write entire cache and drawing layer)
    fn sync(&mut self) -> io::Result<()> {
        self.write_header()?;
        self.data_file.seek(SeekFrom::Start(HEADER_SIZE))?;
        self.data_file.write_all(&self.cache)?;
        self.data_file.sync_data()?;
        
        // Save drawing layer
        std::fs::write("drawing_layer.data", &self.drawing_layer)?;
        
        Ok(())
    }
    
    /// Toggle between Blackboard and Whiteboard modes
    fn toggle_mode(&mut self) -> io::Result<()> {
        let old_bg = self.config.mode.background_color();
        
        self.config.mode = match self.config.mode {
            BoardMode::Blackboard => BoardMode::Whiteboard,
            BoardMode::Whiteboard => BoardMode::Blackboard,
        };
        
        let new_bg = self.config.mode.background_color();
        
        // Remap colors in parallel using rayon for better performance
        self.cache.par_chunks_mut(4).for_each(|pixel| {
            let r = pixel[0];
            let g = pixel[1];
            let b = pixel[2];
            
            // Check if this pixel is the old background color
            if r == old_bg[0] && g == old_bg[1] && b == old_bg[2] {
                // Replace with new background
                pixel[0] = new_bg[0];
                pixel[1] = new_bg[1];
                pixel[2] = new_bg[2];
            } else if r == 0 && g == 0 && b == 0 {
                // Pure black -> white
                pixel[0] = 255;
                pixel[1] = 255;
                pixel[2] = 255;
            } else if r == 255 && g == 255 && b == 255 {
                // Pure white -> black
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 0;
            }
            // All other colors remain unchanged
        });
        
        self.sync()?;
        Ok(())
    }
    
    /// Clear the board with background color (optimized bulk write)
    fn clear(&mut self) -> io::Result<()> {
        let bg_color = self.config.mode.background_color();
        
        println!("Initializing board (this may take a moment)...");
        
        // Fill cache with background color
        for i in (0..self.cache.len()).step_by(4) {
            self.cache[i..i+4].copy_from_slice(&bg_color);
        }
        
        // Clear drawing layer (fully transparent)
        for i in 0..self.drawing_layer.len() {
            self.drawing_layer[i] = 0;
        }
        
        // Reset drawing flag
        self.has_drawings = false;
        
        // Write cache to disk in chunks
        let chunk_size = 1024 * 256; // 256KB chunks
        let total_bytes = self.cache.len();
        let num_chunks = (total_bytes + chunk_size - 1) / chunk_size;
        
        self.data_file.seek(SeekFrom::Start(0))?;
        
        for i in 0..num_chunks {
            let start = i * chunk_size;
            let end = (start + chunk_size).min(total_bytes);
            self.data_file.write_all(&self.cache[start..end])?;
            
            let progress = ((i + 1) * 100 / num_chunks).min(100);
            print!("\\rProgress: {}%", progress);
            io::stdout().flush()?;
        }
        
        println!(" - Complete!");
        self.data_file.sync_all()?;
        Ok(())
    }

    /// Get the default pen color for the current board mode
    fn default_pen_color(&self) -> [u8; 4] {
        self.config.mode.default_pen_color()
    }

    /// Render the current viewport with optional cylindrical projection
    /// Optimized with parallel processing for maximum CPU utilization
    fn render(&mut self, frame: &mut [u8], screen_width: u32, screen_height: u32) -> io::Result<()> {
        // Check if we can reuse the cached viewport
        let needs_rerender = self.viewport_dirty ||
                            self.cached_viewport_width != screen_width ||
                            self.cached_viewport_height != screen_height ||
                            (self.viewport.position.x - self.cached_viewport_pos.x).abs() > 0.001 ||
                            (self.viewport.position.y - self.cached_viewport_pos.y).abs() > 0.001 ||
                            (self.viewport.zoom - self.cached_viewport_zoom).abs() > 0.001;
        
        if !needs_rerender && !self.viewport_cache.is_empty() {
            // Use cached viewport
            frame.copy_from_slice(&self.viewport_cache);
            return Ok(());
        }
        
        // Need to re-render
        let buffer_size = (screen_width * screen_height * 4) as usize;
        if self.viewport_cache.len() != buffer_size {
            self.viewport_cache = vec![0u8; buffer_size];
        }
        
        // Starting position for rendering
        let start_x = self.viewport.position.x as i32;
        let start_y = self.viewport.position.y as i32;
        let zoom = self.viewport.zoom;
        
        let black = [0u8, 0u8, 0u8, 255u8]; // Black for out-of-bounds areas
        let width = self.config.width as i32;
        let height = self.config.height as i32;
        let cache_ptr = &self.cache;
        
        // Parallel row rendering for maximum CPU utilization
        self.viewport_cache.par_chunks_mut((screen_width * 4) as usize)
            .enumerate()
            .for_each(|(screen_y, row)| {
                // Apply zoom: convert screen coords to board coords
                let board_y = start_y + ((screen_y as f32) / zoom) as i32;
                
                if board_y >= 0 && board_y < height {
                    let row_start_offset = (board_y as usize) * (width as usize) * 4;
                    
                    // Process pixels in this row
                    for screen_x in 0..screen_width {
                        let board_x = start_x + ((screen_x as f32) / zoom) as i32;
                        let wrapped_x = board_x.rem_euclid(width) as usize;
                        let src_offset = row_start_offset + (wrapped_x * 4);
                        let dst_offset = (screen_x * 4) as usize;
                        row[dst_offset..dst_offset + 4].copy_from_slice(&cache_ptr[src_offset..src_offset + 4]);
                    }
                } else {
                    // Fill with black if out of vertical bounds
                    for screen_x in 0..screen_width {
                        let dst_offset = (screen_x * 4) as usize;
                        row[dst_offset..dst_offset + 4].copy_from_slice(&black);
                    }
                }
            });
        
        // Update cache metadata
        self.cached_viewport_width = screen_width;
        self.cached_viewport_height = screen_height;
        self.cached_viewport_pos = Point { x: self.viewport.position.x, y: self.viewport.position.y };
        self.cached_viewport_zoom = self.viewport.zoom;
        self.viewport_dirty = false;
        
        // Copy to output frame
        frame.copy_from_slice(&self.viewport_cache);

        Ok(())
    }
    
    /// Render the drawing layer with alpha blending on top of the current frame
    fn render_drawing_layer(&self, frame: &mut [u8], screen_width: u32, _screen_height: u32) {
        // Early exit if no drawings at all
        if !self.has_drawings {
            return;
        }
        
        use rayon::prelude::*;
        
        let start_x = self.viewport.position.x as i32;
        let start_y = self.viewport.position.y as i32;
        let zoom = self.viewport.zoom;
        let width = self.config.width as i32;
        let height = self.config.height as i32;
        
        // Use fixed-point arithmetic for zoom (16.16 fixed point)
        let zoom_inv_fixed = ((1.0 / zoom) * 65536.0) as i32;
        
        // Parallel processing by rows
        frame.par_chunks_mut((screen_width * 4) as usize)
            .enumerate()
            .for_each(|(screen_y, row)| {
                let board_y = start_y + ((screen_y as i32 * zoom_inv_fixed) >> 16);
                
                if board_y < 0 || board_y >= height {
                    return;
                }
                
                let row_start_offset = (board_y as usize) * (width as usize) * 4;
                
                // Process pixels in this row
                for screen_x in 0..screen_width {
                    let board_x = start_x + ((screen_x as i32 * zoom_inv_fixed) >> 16);
                    let wrapped_x = board_x.rem_euclid(width) as usize;
                    let src_offset = row_start_offset + (wrapped_x * 4);
                    let dst_offset = (screen_x * 4) as usize;
                    
                    if src_offset + 3 >= self.drawing_layer.len() || dst_offset + 3 >= row.len() {
                        continue;
                    }
                    
                    let alpha = self.drawing_layer[src_offset + 3];
                    
                    // Skip fully transparent pixels
                    if alpha == 0 {
                        continue;
                    }
                    
                    // Use integer alpha blending
                    if alpha == 255 {
                        // Fully opaque - direct copy
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                self.drawing_layer.as_ptr().add(src_offset),
                                row.as_mut_ptr().add(dst_offset),
                                3
                            );
                        }
                    } else {
                        // Partial transparency - integer blend
                        let inv_alpha = 255 - alpha;
                        row[dst_offset] = ((self.drawing_layer[src_offset] as u16 * alpha as u16 + row[dst_offset] as u16 * inv_alpha as u16) / 255) as u8;
                        row[dst_offset + 1] = ((self.drawing_layer[src_offset + 1] as u16 * alpha as u16 + row[dst_offset + 1] as u16 * inv_alpha as u16) / 255) as u8;
                        row[dst_offset + 2] = ((self.drawing_layer[src_offset + 2] as u16 * alpha as u16 + row[dst_offset + 2] as u16 * inv_alpha as u16) / 255) as u8;
                    }
                }
            });
    }
}

/// Color marker data
struct ColorMarker {
    color: [u8; 4],
    open_image: Vec<u8>,   // RGBA data
    closed_image: Vec<u8>, // RGBA data
    width: u32,
    height: u32,
}

/// Drawing tool state
struct DrawingTool {
    current_color: [u8; 4],
    brush_size: u32,
    is_drawing: bool,
    is_eraser: bool, // True when using eraser (right mouse)
    last_point: Option<Point>,
    selected_marker_index: usize,
}

/// Pinned poster on board
#[derive(Clone, Serialize, Deserialize)]
struct PinnedPoster {
    position: Point,
    image_data: Vec<u8>,  // RGBA pixel data
    width: u32,
    height: u32,
    name: String,
    #[serde(default = "default_scale")]
    scale: f32,  // Scale factor for the poster (1.0 = original size)
}

fn default_scale() -> f32 {
    1.0
}

/// Main application state
struct RickBoard {
    board: Board,
    drawing_tool: DrawingTool,
    markers: Vec<ColorMarker>,
    posters: Vec<PinnedPoster>,
    show_poster_picker: bool,
    available_posters: Vec<(String, String)>, // (name, path)
    placing_poster: Option<(Vec<u8>, u32, u32, String)>, // (image_data, width, height, name) while placing
    selected_poster_index: Option<usize>, // Index of currently selected poster for moving/scaling
    poster_drag_offset: Option<Point>, // Offset from poster position to cursor when dragging
    legend_collapsed: bool, // Whether the legend is collapsed
    legend_offset: f32, // Y offset for collapse animation (0.0 = fully visible, 200.0 = fully hidden)
}

impl RickBoard {
    fn load_marker_image(path: &str) -> io::Result<(Vec<u8>, u32, u32)> {
        let img = image::open(path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let (width, height) = img.dimensions();
        let rgba = img.to_rgba8();
        Ok((rgba.into_raw(), width, height))
    }
    
    fn new(width: u32, height: u32, mode: BoardMode, file_path: &Path) -> io::Result<Self> {
        let board = Board::new(width, height, mode, file_path)?;
        let default_color = board.default_pen_color();
        
        // Load color markers
        let marker_colors = vec![
            ("black", [0, 0, 0, 255]),
            ("white", [255, 255, 255, 255]),
            ("red", [255, 0, 0, 255]),
            ("blue", [30, 144, 255, 255]),      // Dodger blue
            ("green", [0, 255, 0, 255]),
            ("yellow", [255, 255, 0, 255]),
            ("pink", [255, 0, 255, 255]),       // Magenta
        ];
        
        let mut markers = Vec::new();
        for (name, color) in marker_colors {
            let open_path = format!("assetts/{}_marker_open.png", name);
            let closed_path = format!("assetts/{}_marker_closed.png", name);
            
            if let (Ok((open_data, w1, h1)), Ok((closed_data, _w2, _h2))) = 
                (Self::load_marker_image(&open_path), Self::load_marker_image(&closed_path)) {
                markers.push(ColorMarker {
                    color,
                    open_image: open_data,
                    closed_image: closed_data,
                    width: w1,
                    height: h1,
                });
            }
        }
        
        // Find index of default color marker
        let selected_index = markers.iter()
            .position(|m| m.color == default_color)
            .unwrap_or(0);
        
        // Load available posters from posters/ directory
        let mut available_posters = Vec::new();
        if let Ok(entries) = std::fs::read_dir("posters") {
            for entry in entries.flatten() {
                if let Some(path_str) = entry.path().to_str() {
                    if path_str.ends_with(".png") || path_str.ends_with(".jpg") || path_str.ends_with(".jpeg") {
                        if let Some(name) = entry.file_name().to_str() {
                            available_posters.push((name.to_string(), path_str.to_string()));
                        }
                    }
                }
            }
        }
        
        Ok(RickBoard {
            board,
            drawing_tool: DrawingTool {
                current_color: default_color,
                brush_size: 2,
                is_drawing: false,
                is_eraser: false,
                last_point: None,
                selected_marker_index: selected_index,
            },
            markers,
            posters: Vec::new(),
            show_poster_picker: false,
            available_posters,
            placing_poster: None,
            selected_poster_index: None,
            poster_drag_offset: None,
            legend_collapsed: false,
            legend_offset: 0.0,
        })
    }
    
    /// Initialize and load posters from file
    fn init_with_posters(mut self) -> io::Result<Self> {
        self.load_posters()?;
        Ok(self)
    }

    fn start_drawing(&mut self, point: Point, is_eraser: bool) {
        // Save undo state before starting new drawing operation
        self.board.save_undo_state();
        
        self.drawing_tool.is_drawing = true;
        self.drawing_tool.is_eraser = is_eraser;
        self.drawing_tool.last_point = Some(point);
        // Draw initial pixel with brush size
        let _ = self.draw_brush(point);
    }

    fn continue_drawing(&mut self, point: Point) {
        if self.drawing_tool.is_drawing {
            // Draw line from last point to current point for solid strokes
            if let Some(last_point) = self.drawing_tool.last_point {
                // Calculate distance and interpolate to connect points
                let dx = point.x - last_point.x;
                let dy = point.y - last_point.y;
                let distance = (dx * dx + dy * dy).sqrt();
                let steps = distance.ceil().max(1.0) as i32;
                
                // Draw brushes along the line
                for i in 0..=steps {
                    let t = i as f32 / steps as f32;
                    let interp_point = Point {
                        x: last_point.x + dx * t,
                        y: last_point.y + dy * t,
                    };
                    self.draw_brush(interp_point);
                }
            } else {
                self.draw_brush(point);
            }
            self.drawing_tool.last_point = Some(point);
        }
    }
    
    fn draw_brush(&mut self, center: Point) {
        let radius = (self.drawing_tool.brush_size / 2) as i32;
        let cx = center.x as i32;
        let cy = center.y as i32;
        
        // Use background color for eraser, current color for drawing
        let color = if self.drawing_tool.is_eraser {
            self.board.config.mode.background_color()
        } else {
            self.drawing_tool.current_color
        };
        
        // Direct pixel writes without allocation
        for dy in -radius..=radius {
            let dy2 = dy * dy;
            for dx in -radius..=radius {
                if dx * dx + dy2 <= radius * radius {
                    self.board.draw_pixel(cx + dx, cy + dy, color);
                }
            }
        }
    }

    fn stop_drawing(&mut self) {
        self.drawing_tool.is_drawing = false;
        self.drawing_tool.last_point = None;
        // Don't sync on every mouse release - too slow for large boards
        // Data is safely in cache and will sync on mode toggle or app close
    }

    fn clear_board(&mut self) -> io::Result<()> {
        self.board.clear()?;
        self.board.sync()?;
        Ok(())
    }
    
    /// Toggle between Blackboard and Whiteboard modes
    fn toggle_mode(&mut self) -> io::Result<()> {
        // If currently using white pen (index 1), switch to black (index 0)
        // If currently using black pen (index 0), switch to white (index 1)
        if self.drawing_tool.selected_marker_index == 1 {
            self.drawing_tool.selected_marker_index = 0;
            self.drawing_tool.current_color = self.markers[0].color; // Black
        } else if self.drawing_tool.selected_marker_index == 0 {
            self.drawing_tool.selected_marker_index = 1;
            self.drawing_tool.current_color = self.markers[1].color; // White
        }
        
        self.board.toggle_mode()?;
        Ok(())
    }
    
    /// Find poster at given board coordinates (returns index, checks from top to bottom)
    fn find_poster_at(&self, board_x: f32, board_y: f32) -> Option<usize> {
        // Check posters in reverse order (top to bottom)
        for (i, poster) in self.posters.iter().enumerate().rev() {
            let poster_width = poster.width as f32 * poster.scale;
            let poster_height = poster.height as f32 * poster.scale;
            
            if board_x >= poster.position.x && board_x < poster.position.x + poster_width &&
               board_y >= poster.position.y && board_y < poster.position.y + poster_height {
                return Some(i);
            }
        }
        None
    }
    
    /// Toggle legend collapse state
    fn toggle_legend(&mut self) {
        self.legend_collapsed = !self.legend_collapsed;
    }
    
    /// Update legend animation (smooth slide in/out)
    fn update_legend_animation(&mut self) {
        let target_offset = if self.legend_collapsed { 270.0 } else { 0.0 };
        let speed = 15.0; // pixels per frame
        
        if (self.legend_offset - target_offset).abs() > 0.5 {
            if self.legend_offset < target_offset {
                self.legend_offset = (self.legend_offset + speed).min(target_offset);
            } else {
                self.legend_offset = (self.legend_offset - speed).max(target_offset);
            }
        } else {
            self.legend_offset = target_offset;
        }
    }
    
    /// Save posters to JSON file
    fn save_posters(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.posters)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write("posters.json", json)?;
        Ok(())
    }
    
    /// Load posters from JSON file
    fn load_posters(&mut self) -> io::Result<()> {
        if Path::new("posters.json").exists() {
            let json = std::fs::read_to_string("posters.json")?;
            self.posters = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }
        Ok(())
    }
    
    /// Handle dropped file - copy to posters folder and add as poster at drop location
    fn handle_dropped_file(&mut self, path: &PathBuf, screen_x: f64, screen_y: f64) -> io::Result<()> {
        // Check if file is an image
        let extension = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        
        let is_image = match extension.as_deref() {
            Some("png") | Some("jpg") | Some("jpeg") | Some("bmp") | Some("gif") => true,
            _ => false,
        };
        
        if !is_image {
            eprintln!("Dropped file is not a supported image format");
            return Ok(());
        }
        
        // Create posters directory if it doesn't exist
        fs::create_dir_all("posters")?;
        
        // Get filename and create destination path
        let filename = path.file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file path"))?;
        let dest_path = PathBuf::from("posters").join(filename);
        
        // Copy file to posters folder
        fs::copy(path, &dest_path)?;
        println!("Copied {} to posters folder", filename.to_string_lossy());
        
        // Load the image and add as poster at drop location
        if let Ok(img) = image::open(&dest_path) {
            let (width, height) = img.dimensions();
            let rgba = img.to_rgba8();
            let image_data = rgba.into_raw();
            
            // Convert screen coordinates to board coordinates
            let board_x = self.board.viewport.position.x + (screen_x as f32 / self.board.viewport.zoom);
            let board_y = self.board.viewport.position.y + (screen_y as f32 / self.board.viewport.zoom);
            
            let poster = PinnedPoster {
                position: Point { x: board_x, y: board_y },
                image_data,
                width,
                height,
                name: filename.to_string_lossy().to_string(),
                scale: 1.0,
            };
            
            self.posters.push(poster);
            self.save_posters()?;
            
            println!("Added poster '{}' at ({}, {})", filename.to_string_lossy(), board_x, board_y);
        } else {
            eprintln!("Failed to load image: {}", filename.to_string_lossy());
        }
        
        Ok(())
    }
    
    /// Handle click on UI elements, returns true if click was on UI
    fn handle_ui_click(&mut self, x: f64, y: f64, render_height: u32, render_width: u32) -> io::Result<(bool, bool)> {
        // Returns (clicked_on_ui, mode_was_toggled)
        
        // Apply legend offset to y-coordinate for click detection
        let y_offset = -(self.legend_offset as f64);
        let adjusted_y = y - y_offset;
        
        // Check for click on legend collapse/expand area (top bar: x:10-290)
        // When collapsed, check the actual visible screen position
        // When expanded, check the adjusted position
        let is_top_bar_click = if self.legend_collapsed {
            // When collapsed, the visible hint bar is near y:0-20
            x >= 10.0 && x <= 290.0 && y >= 0.0 && y <= 30.0
        } else {
            // When expanded, use adjusted coordinates
            x >= 10.0 && x <= 290.0 && adjusted_y >= 0.0 && adjusted_y <= 20.0
        };
        
        if is_top_bar_click {
            self.toggle_legend();
            return Ok((true, false));
        }
        
        // Only check other UI elements if legend is not fully collapsed
        if self.legend_offset >= 269.0 {
            return Ok((false, false));
        }
        
        // Check if poster picker is open and handle clicks on it
        if self.show_poster_picker {
            let panel_width = 400u32;
            let panel_height = 300u32;
            let panel_x = (render_width / 2).saturating_sub(panel_width / 2);
            let panel_y = (render_height / 2).saturating_sub(panel_height / 2);
            
            // Check if click is within the poster picker panel
            if x >= panel_x as f64 && x <= (panel_x + panel_width) as f64 &&
               y >= panel_y as f64 && y <= (panel_y + panel_height) as f64 {
                // Check which poster was clicked (each poster is 20 pixels tall, starting at y_offset 40)
                let relative_y = (y - panel_y as f64 - 40.0) as i32;
                if relative_y >= 0 {
                    let poster_index = (relative_y / 20) as usize;
                    if poster_index < self.available_posters.len() {
                        // Load the selected poster
                        if let Some((_name, path)) = self.available_posters.get(poster_index) {
                            if let Ok(img) = image::open(path) {
                                let (width, height) = img.dimensions();
                                let rgba = img.to_rgba8();
                                let image_data = rgba.into_raw();
                                let name = self.available_posters[poster_index].0.clone();
                                self.placing_poster = Some((image_data, width, height, name));
                                self.show_poster_picker = false;
                            }
                        }
                    }
                }
                return Ok((true, false));
            }
        }
        
        // Check if click is on mode toggle button (x:20-135, y:170-190) with offset
        if x >= 20.0 && x <= 135.0 && adjusted_y >= 170.0 && adjusted_y <= 190.0 {
            self.toggle_mode()?;
            return Ok((true, true));
        }
        
        // Check if click is on Posters button (x:145-210, y:170-190) with offset
        if x >= 145.0 && x <= 210.0 && adjusted_y >= 170.0 && adjusted_y <= 190.0 {
            self.show_poster_picker = !self.show_poster_picker;
            return Ok((true, false));
        }
        
        // Check if click is on slider (x:20-160, y:150-165) with offset
        if x >= 20.0 && x <= 160.0 && adjusted_y >= 150.0 && adjusted_y <= 165.0 {
            // Calculate brush size from x position
            let slider_x = (x - 20.0).max(0.0).min(140.0);
            self.drawing_tool.brush_size = ((slider_x / 140.0) * 100.0).round() as u32;
            self.drawing_tool.brush_size = self.drawing_tool.brush_size.max(1).min(100);
            return Ok((true, false));
        }
        
        // Check if click is on color markers (bottom-left corner)
        let marker_spacing = 5.0;
        let bottom_margin = -10.0;
        let scale = 0.5; // 50% scale
        
        for (i, marker) in self.markers.iter().enumerate() {
            // Skip black marker in blackboard mode (index 0)
            if self.board.config.mode == BoardMode::Blackboard && i == 0 {
                continue;
            }
            // Skip white marker in whiteboard mode (index 1)
            if self.board.config.mode == BoardMode::Whiteboard && i == 1 {
                continue;
            }
            
            let scaled_width = marker.width as f64 * scale;
            let scaled_height = marker.height as f64 * scale;
            
            let x_pos = marker_spacing + (i as f64) * (scaled_width + marker_spacing);
            let y_pos = render_height as f64 - scaled_height - bottom_margin;
            
            if x >= x_pos && x <= x_pos + scaled_width && 
               y >= y_pos && y <= y_pos + scaled_height {
                // Marker clicked - update selected marker and current color
                self.drawing_tool.selected_marker_index = i;
                self.drawing_tool.current_color = marker.color;
                return Ok((true, false));
            }
        }
        
        Ok((false, false))
    }
    
    /// Render pinned posters as overlay on top of board
    fn render_posters(&self, frame: &mut [u8], width: u32, height: u32) {
        let zoom = self.board.viewport.zoom;
        let board_width = self.board.config.width as f32;
        
        for poster in &self.posters {
            // Apply cylindrical wrapping: calculate wrapped x position
            let wrapped_x = poster.position.x;
            let viewport_x = self.board.viewport.position.x;
            
            // Calculate the difference and wrap it
            let mut dx = wrapped_x - viewport_x;
            while dx < 0.0 {
                dx += board_width;
            }
            while dx >= board_width {
                dx -= board_width;
            }
            
            // Calculate screen position with cylindrical wrapping
            let screen_x = (dx * zoom) as i32;
            let screen_y = ((poster.position.y - self.board.viewport.position.y) * zoom) as i32;
            
            // Calculate scaled poster dimensions (applying both poster scale and viewport zoom)
            let scaled_width = (poster.width as f32 * poster.scale * zoom) as i32;
            let scaled_height = (poster.height as f32 * poster.scale * zoom) as i32;
            
            // Early exit: skip if poster is completely off-screen
            if screen_x + scaled_width < 0 || screen_x >= width as i32 ||
               screen_y + scaled_height < 0 || screen_y >= height as i32 {
                continue;
            }
            
            // Calculate visible bounds to avoid iterating off-screen pixels
            let start_sx = 0.max(-screen_x);
            let start_sy = 0.max(-screen_y);
            let end_sx = scaled_width.min(width as i32 - screen_x);
            let end_sy = scaled_height.min(height as i32 - screen_y);
            
            // Use fixed-point arithmetic for faster scaling (16.16 fixed point)
            let scale_factor_inv = ((1.0 / (poster.scale * zoom)) * 65536.0) as i32;
            
            // Render poster pixels with scaling (only visible portion)
            for sy in start_sy..end_sy {
                let screen_py = screen_y + sy;
                let poster_py = ((sy * scale_factor_inv) >> 16) as u32;
                
                if poster_py >= poster.height {
                    continue;
                }
                
                let poster_row_base = (poster_py * poster.width * 4) as usize;
                let screen_row_base = (screen_py * width as i32) as usize * 4;
                
                for sx in start_sx..end_sx {
                    let poster_px = ((sx * scale_factor_inv) >> 16) as u32;
                    
                    if poster_px >= poster.width {
                        continue;
                    }
                    
                    let poster_offset = poster_row_base + (poster_px * 4) as usize;
                    
                    // Skip if out of bounds or fully transparent
                    if poster_offset + 3 >= poster.image_data.len() {
                        continue;
                    }
                    
                    let alpha = poster.image_data[poster_offset + 3];
                    if alpha == 0 {
                        continue;
                    }
                    
                    let screen_offset = screen_row_base + ((screen_x + sx) * 4) as usize;
                    if screen_offset + 3 >= frame.len() {
                        continue;
                    }
                    
                    // Alpha blend the poster with the background
                    if alpha == 255 {
                        // Fully opaque - direct copy (most common case)
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                poster.image_data.as_ptr().add(poster_offset),
                                frame.as_mut_ptr().add(screen_offset),
                                3
                            );
                        }
                        frame[screen_offset + 3] = 255;
                    } else {
                        // Partial transparency - blend (using integer math)
                        let inv_alpha = 255 - alpha;
                        
                        frame[screen_offset] = ((poster.image_data[poster_offset] as u16 * alpha as u16 + frame[screen_offset] as u16 * inv_alpha as u16) / 255) as u8;
                        frame[screen_offset + 1] = ((poster.image_data[poster_offset + 1] as u16 * alpha as u16 + frame[screen_offset + 1] as u16 * inv_alpha as u16) / 255) as u8;
                        frame[screen_offset + 2] = ((poster.image_data[poster_offset + 2] as u16 * alpha as u16 + frame[screen_offset + 2] as u16 * inv_alpha as u16) / 255) as u8;
                        frame[screen_offset + 3] = 255;
                    }
                }
            }
        }
    }
    
    /// Render UI overlay (legend and brush controls)
    fn render_ui_overlay(&self, frame: &mut [u8], width: u32, height: u32, fps: f32) {
        let text_color = match self.board.config.mode {
            BoardMode::Blackboard => [255u8, 255u8, 255u8, 255u8], // White text
            BoardMode::Whiteboard => [0u8, 0u8, 0u8, 255u8], // Black text
        };
        
        // Different transparency for different modes
        let bg_color = match self.board.config.mode {
            BoardMode::Blackboard => [0u8, 0u8, 0u8, 128u8], // 50% transparent black
            BoardMode::Whiteboard => [255u8, 255u8, 255u8, 153u8], // 60% transparent white
        };
        
        // Apply collapse animation offset
        let y_offset = -(self.legend_offset as i32);
        
        // Draw background panel (top-left, from y:0 to y:280, 290 pixels wide)
        let bg_alpha = bg_color[3];
        let inv_bg_alpha = 255 - bg_alpha;
        
        for y in 0..280 {
            let screen_y = y + y_offset;
            if screen_y < 0 || screen_y >= height as i32 { continue; }
            let row_offset = (screen_y as u32 * width * 4) as usize;
            
            for x in 10..290 {
                let offset = row_offset + (x * 4) as usize;
                if offset + 3 < frame.len() {
                    // Alpha blend with existing content using integer math
                    frame[offset] = ((bg_color[0] as u16 * bg_alpha as u16 + frame[offset] as u16 * inv_bg_alpha as u16) / 255) as u8;
                    frame[offset + 1] = ((bg_color[1] as u16 * bg_alpha as u16 + frame[offset + 1] as u16 * inv_bg_alpha as u16) / 255) as u8;
                    frame[offset + 2] = ((bg_color[2] as u16 * bg_alpha as u16 + frame[offset + 2] as u16 * inv_bg_alpha as u16) / 255) as u8;
                    frame[offset + 3] = 255; // Keep fully opaque
                }
            }
        }
        
        // Helper to draw text with y-offset
        let draw_text = |f: &mut [u8], w: u32, x: u32, y: u32, text: &str, color: [u8; 4]| {
            let screen_y = y as i32 + y_offset;
            if screen_y >= 0 && screen_y < height as i32 {
                self.draw_simple_text(f, w, x, screen_y as u32, text, color);
            }
        };
        
        // Render text legend (simplified - just draw simple characters)
        draw_text(frame, width, 20, 20, "CONTROLS:", text_color);
        draw_text(frame, width, 20, 35, "Left Click: Draw", text_color);
        draw_text(frame, width, 20, 48, "Right Click: Erase", text_color);
        draw_text(frame, width, 20, 61, "WASD: Pan", text_color);
        draw_text(frame, width, 20, 74, "Mouse Wheel: Zoom", text_color);
        draw_text(frame, width, 20, 87, "+ - Keys: Brush Size", text_color);
        draw_text(frame, width, 20, 100, "C Key: Clear Board", text_color);
        draw_text(frame, width, 20, 113, "P Key: Save", text_color);
        draw_text(frame, width, 20, 126, "ESC: Exit", text_color);
        
        // Draw FPS in top-right corner of legend panel
        let fps_text = format!("FPS: {:.1}", fps);
        draw_text(frame, width, 210, 20, &fps_text, text_color);
        
        // Draw brush size slider
        draw_text(frame, width, 20, 139, &format!("Brush: {}", self.drawing_tool.brush_size), text_color);
        
        // Draw slider bar (140 pixels wide) with offset
        for x in 20..160 {
            for dy in 0..3 {
                let screen_y = 155 + dy + y_offset;
                if screen_y >= 0 && screen_y < height as i32 {
                    let offset = ((screen_y as u32 * width + x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&text_color);
                    }
                }
            }
        }
        
        // Draw slider position indicator with offset
        let slider_pos = 20 + ((self.drawing_tool.brush_size.min(100) * 140) / 100) as u32;
        for dy in -5..=5 {
            for dx in -2..=2 {
                let py = 156 + dy + y_offset;
                let px = slider_pos as i32 + dx;
                if px >= 0 && py >= 0 && py < height as i32 {
                    let offset = ((py as u32 * width + px as u32) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&[255, 100, 100, 255]);
                    }
                }
            }
        }
        
        // Draw brush preview circle with offset
        let preview_x = 210;
        let preview_y = 86;
        let radius = (self.drawing_tool.brush_size / 2).min(50) as i32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy <= radius * radius {
                    let px = preview_x + dx;
                    let py = preview_y + dy + y_offset;
                    if px >= 0 && py >= 0 && py < height as i32 {
                        let offset = ((py as u32 * width + px as u32) * 4) as usize;
                        if offset + 3 < frame.len() {
                            frame[offset..offset + 4].copy_from_slice(&text_color);
                        }
                    }
                }
            }
        }
        
        // Draw mode toggle button
        let button_text = match self.board.config.mode {
            BoardMode::Blackboard => "Mode: Blackboard",
            BoardMode::Whiteboard => "Mode: Whiteboard",
        };
        draw_text(frame, width, 30, 175, button_text, text_color);
        
        // Draw button border (clickable area: x:20-135, y:170-190) with offset
        for x in 20..135 {
            for y in [170, 189].iter() {
                let screen_y = *y as i32 + y_offset;
                if screen_y >= 0 && screen_y < height as i32 {
                    let offset = ((screen_y as u32 * width + x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&text_color);
                    }
                }
            }
        }
        for y in 170..190 {
            let screen_y = y as i32 + y_offset;
            if screen_y >= 0 && screen_y < height as i32 {
                for x in [20, 134].iter() {
                    let offset = ((screen_y as u32 * width + *x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&text_color);
                    }
                }
            }
        }
        
        // Draw Posters button (next to mode button)
        draw_text(frame, width, 150, 175, "Posters", text_color);
        
        // Draw button border (clickable area: x:145-210, y:170-190) with offset
        for x in 145..210 {
            for y in [170, 189].iter() {
                let screen_y = *y as i32 + y_offset;
                if screen_y >= 0 && screen_y < height as i32 {
                    let offset = ((screen_y as u32 * width + x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&text_color);
                    }
                }
            }
        }
        for y in 170..190 {
            let screen_y = y as i32 + y_offset;
            if screen_y >= 0 && screen_y < height as i32 {
                for x in [145, 209].iter() {
                    let offset = ((screen_y as u32 * width + *x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&text_color);
                    }
                }
            }
        }
        
        // Draw poster controls help text
        draw_text(frame, width, 20, 205, "Poster Controls:", text_color);
        draw_text(frame, width, 20, 220, "Ctrl+Click: Move", text_color);
        draw_text(frame, width, 20, 235, "Ctrl+Wheel: Scale", text_color);
        draw_text(frame, width, 20, 250, "Ctrl+RClick: Delete", text_color);
        
        // Draw collapse/expand hint at top
        let hint_text = if self.legend_collapsed { "Click to show" } else { "Click to hide" };
        draw_text(frame, width, 100, 5, hint_text, text_color);
        
        // Render color markers at bottom-left corner
        self.render_markers(frame, width, height);
        
        // Render poster picker if active
        if self.show_poster_picker {
            self.render_poster_picker(frame, width, height);
        }
    }
    
    /// Render poster picker overlay
    fn render_poster_picker(&self, frame: &mut [u8], width: u32, height: u32) {
        let text_color = match self.board.config.mode {
            BoardMode::Blackboard => [255u8, 255u8, 255u8, 255u8],
            BoardMode::Whiteboard => [0u8, 0u8, 0u8, 255u8],
        };
        
        let bg_color = match self.board.config.mode {
            BoardMode::Blackboard => [0u8, 0u8, 0u8, 200u8],
            BoardMode::Whiteboard => [255u8, 255u8, 255u8, 200u8],
        };
        
        // Draw semi-transparent overlay panel (center of screen)
        let panel_width = 400u32;
        let panel_height = 300u32;
        let panel_x = (width / 2).saturating_sub(panel_width / 2);
        let panel_y = (height / 2).saturating_sub(panel_height / 2);
        
        let panel_alpha = bg_color[3];
        let panel_inv_alpha = 255 - panel_alpha;
        
        for y in panel_y..panel_y + panel_height {
            for x in panel_x..panel_x + panel_width {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 3 < frame.len() {
                    frame[offset] = ((bg_color[0] as u16 * panel_alpha as u16 + frame[offset] as u16 * panel_inv_alpha as u16) / 255) as u8;
                    frame[offset + 1] = ((bg_color[1] as u16 * panel_alpha as u16 + frame[offset + 1] as u16 * panel_inv_alpha as u16) / 255) as u8;
                    frame[offset + 2] = ((bg_color[2] as u16 * panel_alpha as u16 + frame[offset + 2] as u16 * panel_inv_alpha as u16) / 255) as u8;
                    frame[offset + 3] = 255;
                }
            }
        }
        
        // Draw border
        for x in panel_x..panel_x + panel_width {
            for y in [panel_y, panel_y + panel_height - 1].iter() {
                let offset = ((*y * width + x) * 4) as usize;
                if offset + 3 < frame.len() {
                    frame[offset..offset + 4].copy_from_slice(&text_color);
                }
            }
        }
        for y in panel_y..panel_y + panel_height {
            for x in [panel_x, panel_x + panel_width - 1].iter() {
                let offset = ((y * width + *x) * 4) as usize;
                if offset + 3 < frame.len() {
                    frame[offset..offset + 4].copy_from_slice(&text_color);
                }
            }
        }
        
        // Draw title
        self.draw_simple_text(frame, width, panel_x + 10, panel_y + 10, "Select a Poster:", text_color);
        
        // List available posters
        let mut y_offset = 40;
        for (i, (name, _path)) in self.available_posters.iter().enumerate() {
            let display_text = format!("{}. {}", i + 1, name);
            self.draw_simple_text(frame, width, panel_x + 20, panel_y + y_offset, &display_text, text_color);
            y_offset += 20;
        }
        
        self.draw_simple_text(frame, width, panel_x + 10, panel_y + panel_height - 25, "Click poster name to select", text_color);
    }
    
    /// Render save progress bar at top center
    fn render_save_progress(&self, frame: &mut [u8], width: u32, time_until_save: f32, is_saving: bool) {
        let bar_width = 200u32;
        let bar_height = 6u32;
        let bar_x = (width / 2) - (bar_width / 2);
        let bar_y = 10u32;
        
        let text_color = match self.board.config.mode {
            BoardMode::Blackboard => [220, 220, 220, 255],
            BoardMode::Whiteboard => [40, 40, 40, 255],
        };
        
        let bg_color = match self.board.config.mode {
            BoardMode::Blackboard => [0u8, 0u8, 0u8, 128u8], // 50% transparent black
            BoardMode::Whiteboard => [255u8, 255u8, 255u8, 153u8], // 60% transparent white
        };
        
        // Draw progress bar background (empty)
        for y in bar_y..bar_y + bar_height {
            for x in bar_x..bar_x + bar_width {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 3 < frame.len() {
                    frame[offset] = text_color[0] / 3;
                    frame[offset + 1] = text_color[1] / 3;
                    frame[offset + 2] = text_color[2] / 3;
                    frame[offset + 3] = 255;
                }
            }
        }
        
        // Draw progress bar fill (elapsed time)
        let progress = (60.0 - time_until_save) / 60.0; // 60 seconds = 1 minute
        let fill_width = (bar_width as f32 * progress) as u32;
        for y in bar_y..bar_y + bar_height {
            for x in bar_x..bar_x + fill_width {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 3 < frame.len() {
                    frame[offset..offset + 4].copy_from_slice(&text_color);
                }
            }
        }
        
        // Show "Saving..." message under progress bar when saving
        if is_saving {
            let msg_y = bar_y + bar_height + 5; // 5 pixels below progress bar
            let msg_width = 80u32;
            let msg_height = 15u32;
            let msg_x = bar_x + (bar_width / 2) - (msg_width / 2);
            
            // Draw background panel for message
            let msg_alpha = bg_color[3];
            let msg_inv_alpha = 255 - msg_alpha;
            
            for y in msg_y..msg_y + msg_height {
                for x in msg_x..msg_x + msg_width {
                    let offset = ((y * width + x) * 4) as usize;
                    if offset + 3 < frame.len() {
                        // Alpha blend with existing content using integer math
                        frame[offset] = ((bg_color[0] as u16 * msg_alpha as u16 + frame[offset] as u16 * msg_inv_alpha as u16) / 255) as u8;
                        frame[offset + 1] = ((bg_color[1] as u16 * msg_alpha as u16 + frame[offset + 1] as u16 * msg_inv_alpha as u16) / 255) as u8;
                        frame[offset + 2] = ((bg_color[2] as u16 * msg_alpha as u16 + frame[offset + 2] as u16 * msg_inv_alpha as u16) / 255) as u8;
                        frame[offset + 3] = 255;
                    }
                }
            }
            
            // Draw "Saving..." text centered
            self.draw_simple_text(frame, width, msg_x + 8, msg_y + 3, "Saving...", text_color);
        }
    }
    
    /// Render color markers at bottom-left
    fn render_markers(&self, frame: &mut [u8], width: u32, height: u32) {
        let marker_spacing = 5u32; // 5 pixels between markers
        let bottom_margin = -10i32; // Negative to extend below bottom edge
        let scale = 0.5; // 50% scale
        
        for (i, marker) in self.markers.iter().enumerate() {
            let is_selected = i == self.drawing_tool.selected_marker_index;
            let image_data = if is_selected { &marker.open_image } else { &marker.closed_image };
            
            let scaled_width = (marker.width as f32 * scale) as u32;
            let scaled_height = (marker.height as f32 * scale) as u32;
            
            // Calculate position (bottom-left corner, arranged in a row)
            let x_pos = marker_spacing + (i as u32) * (scaled_width + marker_spacing);
            let y_pos = (height as i32 - scaled_height as i32 - bottom_margin) as u32;
            
            // Render marker image with scaling
            for sy in 0..scaled_height {
                for sx in 0..scaled_width {
                    // Map scaled coordinates back to original image
                    let mx = (sx as f32 / scale) as u32;
                    let my = (sy as f32 / scale) as u32;
                    
                    let img_offset = ((my * marker.width + mx) * 4) as usize;
                    let screen_x = x_pos + sx;
                    let screen_y = y_pos + sy;
                    
                    if screen_x < width && screen_y < height && img_offset + 3 < image_data.len() {
                        let frame_offset = ((screen_y * width + screen_x) * 4) as usize;
                        if frame_offset + 3 < frame.len() {
                            let alpha = image_data[img_offset + 3];
                            if alpha > 0 {
                                let inv_alpha = 255 - alpha;
                                frame[frame_offset] = ((image_data[img_offset] as u16 * alpha as u16 + frame[frame_offset] as u16 * inv_alpha as u16) / 255) as u8;
                                frame[frame_offset + 1] = ((image_data[img_offset + 1] as u16 * alpha as u16 + frame[frame_offset + 1] as u16 * inv_alpha as u16) / 255) as u8;
                                frame[frame_offset + 2] = ((image_data[img_offset + 2] as u16 * alpha as u16 + frame[frame_offset + 2] as u16 * inv_alpha as u16) / 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    
    /// Draw simple text (basic bitmap font)
    fn draw_simple_text(&self, frame: &mut [u8], width: u32, x: u32, y: u32, text: &str, color: [u8; 4]) {
        for (i, ch) in text.chars().enumerate() {
            let char_x = x + (i as u32 * 6);
            self.draw_char(frame, width, char_x, y, ch, color);
        }
    }
    
    /// Draw a single character (very simple 5x7 bitmap)
    fn draw_char(&self, frame: &mut [u8], width: u32, x: u32, y: u32, ch: char, color: [u8; 4]) {
        // Simple pixel patterns for basic characters
        let pattern: &[u8] = match ch {
            'A' | 'a' => &[0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
            'B' | 'b' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
            'C' | 'c' => &[0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
            'D' | 'd' => &[0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
            'E' | 'e' => &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
            'F' | 'f' => &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
            'G' | 'g' => &[0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
            'H' | 'h' => &[0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
            'I' | 'i' => &[0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
            'K' | 'k' => &[0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
            'L' | 'l' => &[0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
            'M' | 'm' => &[0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
            'N' | 'n' => &[0b10001, 0b11001, 0b10101, 0b10101, 0b10011, 0b10001, 0b10001],
            'O' | 'o' => &[0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
            'P' | 'p' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
            'R' | 'r' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
            'S' | 's' => &[0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
            'T' | 't' => &[0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
            'U' | 'u' => &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
            'W' | 'w' => &[0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
            'X' | 'x' => &[0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
            'Y' | 'y' => &[0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
            'Z' | 'z' => &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
            '0' => &[0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
            '1' => &[0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
            '2' => &[0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
            '3' => &[0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
            '4' => &[0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
            '5' => &[0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
            '6' => &[0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
            '7' => &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
            '8' => &[0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
            '9' => &[0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
            ':' => &[0b00000, 0b00100, 0b00000, 0b00000, 0b00000, 0b00100, 0b00000],
            '+' => &[0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
            '-' | '/' => &[0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
            ' ' => &[0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
            _ => &[0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
        };
        
        for (row, &bits) in pattern.iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 == 1 {
                    let px = x + col;
                    let py = y + row as u32;
                    let offset = ((py * width + px) * 4) as usize;
                    if offset + 3 < frame.len() {
                        frame[offset..offset + 4].copy_from_slice(&color);
                    }
                }
            }
        }
    }
}

struct App {
    window: Option<Rc<Window>>,
    pixels: Option<Pixels<'static>>,
    rickboard: RickBoard,
    mouse_down: bool,
    right_mouse_down: bool, // Track right mouse button for eraser
    cursor_pos: (f64, f64), // Track cursor position for zoom
    render_width: u32,
    render_height: u32,
    frame_count: u32,
    last_fps_update: Instant,
    fps: f32,
    last_save: Instant,
    is_saving: bool,
    has_unsaved_changes: bool,
    modifiers: ModifiersState,
    save_message_until: Option<Instant>, // Show saving message until this time
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {        if self.pixels.is_none() {
            let window_attrs = Window::default_attributes()
                .with_title("RickBoard - Virtual Blackboard/Whiteboard")
                .with_inner_size(winit::dpi::LogicalSize::new(1024u32, 768u32));
            
            let window = Rc::new(event_loop.create_window(window_attrs).unwrap());
            let window_size = window.inner_size();
            
            // Leak an Rc clone to create a 'static reference for Pixels
            let window_clone = Rc::clone(&window);
            let window_ref: &'static Window = unsafe { &*(Rc::into_raw(window_clone) as *const Window) };
            let surface_texture = SurfaceTexture::new(window_size.width, window_size.height, window_ref);
            let pixels = Pixels::new(window_size.width, window_size.height, surface_texture).unwrap();
            
            self.render_width = window_size.width;
            self.render_height = window_size.height;
            
            self.window = Some(window);
            self.pixels = Some(pixels);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                println!("Closing RickBoard...");
                let _ = self.rickboard.board.sync();
                let _ = self.rickboard.save_posters();
                event_loop.exit();
            }
            
            WindowEvent::Resized(new_size) => {
                if let Some(pixels) = &mut self.pixels {
                    if let Err(e) = pixels.resize_surface(new_size.width, new_size.height) {
                        eprintln!("Failed to resize surface: {}", e);
                    }
                    if let Err(e) = pixels.resize_buffer(new_size.width, new_size.height) {
                        eprintln!("Failed to resize buffer: {}", e);
                    }
                    self.render_width = new_size.width;
                    self.render_height = new_size.height;
                }
            }
            
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers.state();
            }
            
            WindowEvent::MouseInput { state, button, .. } => {
                match button {
                    MouseButton::Left => {
                        match state {
                            ElementState::Pressed => {
                                // Check if click is on UI first
                                if let Ok((on_ui, mode_toggled)) = self.rickboard.handle_ui_click(self.cursor_pos.0, self.cursor_pos.1, self.render_height, self.render_width) {
                                    if mode_toggled {
                                        self.has_unsaved_changes = true;
                                    }
                                    if !on_ui {
                                        // Check if we're placing a poster
                                        if let Some((image_data, width, height, name)) = self.rickboard.placing_poster.take() {
                                            // Convert screen coords to board coords
                                            let board_x = self.rickboard.board.viewport.position.x + self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom;
                                            let board_y = self.rickboard.board.viewport.position.y + self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom;
                                            
                                            self.rickboard.posters.push(PinnedPoster {
                                                position: Point { x: board_x, y: board_y },
                                                image_data,
                                                width,
                                                height,
                                                name,
                                                scale: 1.0,
                                            });
                                            self.has_unsaved_changes = true;
                                        } else if self.modifiers.control_key() {
                                            // Ctrl+Click to select/move poster
                                            let board_x = self.rickboard.board.viewport.position.x + self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom;
                                            let board_y = self.rickboard.board.viewport.position.y + self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom;
                                            
                                            if let Some(poster_idx) = self.rickboard.find_poster_at(board_x, board_y) {
                                                self.rickboard.selected_poster_index = Some(poster_idx);
                                                // Calculate drag offset
                                                let poster = &self.rickboard.posters[poster_idx];
                                                self.rickboard.poster_drag_offset = Some(Point {
                                                    x: board_x - poster.position.x,
                                                    y: board_y - poster.position.y,
                                                });
                                            } else {
                                                self.rickboard.selected_poster_index = None;
                                                self.rickboard.poster_drag_offset = None;
                                            }
                                        } else {
                                            self.mouse_down = true;
                                        }
                                    }
                                    if let Some(window) = &self.window {
                                        window.request_redraw();
                                    }
                                }
                            }
                            ElementState::Released => {
                                self.mouse_down = false;
                                self.rickboard.stop_drawing();
                                // Release poster drag
                                if self.rickboard.selected_poster_index.is_some() {
                                    self.rickboard.selected_poster_index = None;
                                    self.rickboard.poster_drag_offset = None;
                                    self.has_unsaved_changes = true;
                                }
                            }
                        }
                    }
                    MouseButton::Right => {
                        match state {
                            ElementState::Pressed => {
                                if self.modifiers.control_key() {
                                    // Ctrl+Right Click to delete poster
                                    let board_x = self.rickboard.board.viewport.position.x + self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom;
                                    let board_y = self.rickboard.board.viewport.position.y + self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom;
                                    
                                    if let Some(poster_idx) = self.rickboard.find_poster_at(board_x, board_y) {
                                        self.rickboard.posters.remove(poster_idx);
                                        self.has_unsaved_changes = true;
                                        if let Some(window) = &self.window {
                                            window.request_redraw();
                                        }
                                    }
                                } else {
                                    self.right_mouse_down = true;
                                }
                            }
                            ElementState::Released => {
                                self.right_mouse_down = false;
                                self.rickboard.stop_drawing();
                            }
                        }
                    }
                    _ => {}
                }
            }
            
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                
                // Move poster if one is selected
                if let (Some(poster_idx), Some(offset)) = (self.rickboard.selected_poster_index, self.rickboard.poster_drag_offset) {
                    let board_x = self.rickboard.board.viewport.position.x + self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom;
                    let board_y = self.rickboard.board.viewport.position.y + self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom;
                    
                    if let Some(poster) = self.rickboard.posters.get_mut(poster_idx) {
                        poster.position.x = board_x - offset.x;
                        poster.position.y = board_y - offset.y;
                    }
                    
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return; // Don't draw on board while dragging poster
                }
                
                // Handle slider dragging
                if self.mouse_down && position.x >= 20.0 && position.x <= 160.0 && position.y >= 150.0 && position.y <= 165.0 {
                    let _ = self.rickboard.handle_ui_click(position.x, position.y, self.render_height, self.render_width);
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                    return; // Don't draw on board while dragging slider
                }
                
                if self.mouse_down || self.right_mouse_down {
                    // Convert screen coordinates to board coordinates with proper zoom handling
                    let board_x = self.rickboard.board.viewport.position.x + (position.x as f32 / self.rickboard.board.viewport.zoom);
                    let board_y = self.rickboard.board.viewport.position.y + (position.y as f32 / self.rickboard.board.viewport.zoom);
                    let is_eraser = self.right_mouse_down;
                    
                    if !self.rickboard.drawing_tool.is_drawing {
                        self.rickboard.start_drawing(Point { x: board_x, y: board_y }, is_eraser);
                    } else {
                        self.rickboard.continue_drawing(Point { x: board_x, y: board_y });
                    }
                    self.has_unsaved_changes = true;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            
            WindowEvent::MouseWheel { delta, .. } => {
                if self.modifiers.control_key() {
                    // Ctrl+Wheel: Scale selected poster
                    let delta_y = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(pos) => (pos.y / 20.0) as f32,
                    };
                    
                    let board_x = self.rickboard.board.viewport.position.x + self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom;
                    let board_y = self.rickboard.board.viewport.position.y + self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom;
                    
                    if let Some(poster_idx) = self.rickboard.find_poster_at(board_x, board_y) {
                        if let Some(poster) = self.rickboard.posters.get_mut(poster_idx) {
                            let scale_factor = if delta_y > 0.0 { 1.1 } else { 0.9 };
                            poster.scale = (poster.scale * scale_factor).clamp(0.1, 10.0);
                            self.has_unsaved_changes = true;
                            
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                    }
                } else {
                    // Normal wheel: Zoom viewport
                    let zoom_factor = match delta {
                        MouseScrollDelta::LineDelta(_, y) => {
                            if y > 0.0 { 1.1 } else { 0.9 }
                        }
                        MouseScrollDelta::PixelDelta(pos) => {
                            if pos.y > 0.0 { 1.1 } else { 0.9 }
                        }
                    };
                    
                    // Calculate board position at cursor before zoom
                    let cursor_board_x = self.rickboard.board.viewport.position.x + (self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom);
                    let cursor_board_y = self.rickboard.board.viewport.position.y + (self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom);
                    
                    // Apply zoom with limit
                    self.rickboard.board.viewport.zoom = (self.rickboard.board.viewport.zoom * zoom_factor).clamp(0.1, 1.5);
                    
                    // Adjust viewport position to keep cursor at same board position
                    self.rickboard.board.viewport.position.x = cursor_board_x - (self.cursor_pos.0 as f32 / self.rickboard.board.viewport.zoom);
                    self.rickboard.board.viewport.position.y = cursor_board_y - (self.cursor_pos.1 as f32 / self.rickboard.board.viewport.zoom);
                    
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let PhysicalKey::Code(keycode) = event.physical_key {
                        match keycode {
                            KeyCode::Escape => event_loop.exit(),
                            KeyCode::KeyW => {
                                self.rickboard.board.viewport.position.y -= 50.0;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyS => {
                                self.rickboard.board.viewport.position.y += 50.0;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyA => {
                                self.rickboard.board.viewport.position.x -= 50.0;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyD => {
                                self.rickboard.board.viewport.position.x += 50.0;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::Equal | KeyCode::NumpadAdd => {
                                self.rickboard.drawing_tool.brush_size = (self.rickboard.drawing_tool.brush_size + 1).min(100);
                                println!("Brush size: {}", self.rickboard.drawing_tool.brush_size);
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::Minus | KeyCode::NumpadSubtract => {
                                self.rickboard.drawing_tool.brush_size = (self.rickboard.drawing_tool.brush_size.saturating_sub(1)).max(1);
                                println!("Brush size: {}", self.rickboard.drawing_tool.brush_size);
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyC => {
                                if let Err(e) = self.rickboard.clear_board() {
                                    eprintln!("Clear error: {}", e);
                                }
                                self.has_unsaved_changes = true;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyP => {
                                self.is_saving = true;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                                if let Err(e) = self.rickboard.board.sync() {
                                    eprintln!("Save error: {}", e);
                                } else {
                                    self.has_unsaved_changes = false;
                                }
                                // Save posters
                                if let Err(e) = self.rickboard.save_posters() {
                                    eprintln!("Poster save error: {}", e);
                                }
                                self.last_save = Instant::now(); // Reset timer
                                self.save_message_until = Some(Instant::now() + std::time::Duration::from_millis(500));
                                self.is_saving = false;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            KeyCode::KeyZ => {
                                // Ctrl+Z for undo
                                if self.modifiers.control_key() {
                                    if self.rickboard.board.undo() {
                                        println!("Undo successful");
                                        self.has_unsaved_changes = true;
                                        if let Some(window) = &self.window {
                                            window.request_redraw();
                                        }
                                    } else {
                                        println!("Nothing to undo");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            
            WindowEvent::DroppedFile(path) => {
                // Handle dropped image file
                if let Err(e) = self.rickboard.handle_dropped_file(&path, self.cursor_pos.0, self.cursor_pos.1) {
                    eprintln!("Error handling dropped file: {}", e);
                }
            }
            
            WindowEvent::RedrawRequested => {
                // Update legend animation
                self.rickboard.update_legend_animation();
                
                // Update FPS counter
                self.frame_count += 1;
                let elapsed = self.last_fps_update.elapsed();
                if elapsed.as_secs_f32() >= 1.0 {
                    self.fps = self.frame_count as f32 / elapsed.as_secs_f32();
                    self.frame_count = 0;
                    self.last_fps_update = Instant::now();
                }
                
                // Check for auto-save (every 1 minute, only if changes made)
                let time_since_save = self.last_save.elapsed().as_secs_f32();
                if time_since_save >= 60.0 && !self.is_saving && self.has_unsaved_changes {
                    self.is_saving = true;
                    if let Err(e) = self.rickboard.board.sync() {
                        eprintln!("Auto-save error: {}", e);
                    } else {
                        self.has_unsaved_changes = false;
                    }
                    // Save posters
                    if let Err(e) = self.rickboard.save_posters() {
                        eprintln!("Auto-save poster error: {}", e);
                    }
                    self.last_save = Instant::now();
                    self.save_message_until = Some(Instant::now() + std::time::Duration::from_millis(500));
                    self.is_saving = false;
                }
                
                // Check if save message should still be displayed
                let show_save_message = if let Some(until) = self.save_message_until {
                    if Instant::now() < until {
                        true
                    } else {
                        self.save_message_until = None;
                        false
                    }
                } else {
                    self.is_saving
                };
                
                if let Some(pixels) = &mut self.pixels {
                    let frame = pixels.frame_mut();
                    
                    let frame_start = Instant::now();
                    
                    // Render the board's viewport to the screen
                    let t0 = Instant::now();
                    if let Err(e) = self.rickboard.board.render(frame, self.render_width, self.render_height) {
                        eprintln!("Board render error: {}", e);
                    }
                    let board_time = t0.elapsed();
                    
                    // Render posters on top of board background
                    let t1 = Instant::now();
                    self.rickboard.render_posters(frame, self.render_width, self.render_height);
                    let poster_time = t1.elapsed();
                    
                    // Render drawing layer on top of posters
                    let t2 = Instant::now();
                    self.rickboard.board.render_drawing_layer(frame, self.render_width, self.render_height);
                    let drawing_time = t2.elapsed();
                    
                    // Render UI overlay on top
                    let t3 = Instant::now();
                    self.rickboard.render_ui_overlay(frame, self.render_width, self.render_height, self.fps);
                    let ui_time = t3.elapsed();
                    
                    // Render save progress bar
                    let t4 = Instant::now();
                    let time_until_save = (60.0 - time_since_save).max(0.0);
                    self.rickboard.render_save_progress(frame, self.render_width, time_until_save, show_save_message);
                    let progress_time = t4.elapsed();
                    
                    // Present to screen
                    let t5 = Instant::now();
                    if let Err(e) = pixels.render() {
                        eprintln!("Render error: {}", e);
                    }
                    let present_time = t5.elapsed();
                    
                    let total_time = frame_start.elapsed();
                    
                    // Print timing every 60 frames
                    if self.frame_count % 60 == 0 {
                        println!("Frame time: {:.2}ms (board: {:.2}ms, posters: {:.2}ms, drawing: {:.2}ms, ui: {:.2}ms, progress: {:.2}ms, present: {:.2}ms)",
                            total_time.as_secs_f32() * 1000.0,
                            board_time.as_secs_f32() * 1000.0,
                            poster_time.as_secs_f32() * 1000.0,
                            drawing_time.as_secs_f32() * 1000.0,
                            ui_time.as_secs_f32() * 1000.0,
                            progress_time.as_secs_f32() * 1000.0,
                            present_time.as_secs_f32() * 1000.0
                        );
                    }
                }
                
                // Request another redraw to keep the display updated
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            
            _ => {}
        }
    }
}

fn main() {
    // Default to Blackboard mode (can be changed via UI button)
    let mode = BoardMode::Blackboard;
    
    let board_path = Path::new("rickboard.data");
    
    match RickBoard::new(80000, 1000, mode, board_path).and_then(|rb| rb.init_with_posters()) {
        Ok(rickboard) => {
            let event_loop = EventLoop::new().unwrap();
            event_loop.set_control_flow(ControlFlow::Wait);
            
            let mut app = App {
                window: None,
                pixels: None,
                rickboard,
                mouse_down: false,
                right_mouse_down: false,
                cursor_pos: (0.0, 0.0),
                render_width: 1024,
                render_height: 768,
                frame_count: 0,
                last_fps_update: Instant::now(),
                fps: 0.0,
                last_save: Instant::now(),
                is_saving: false,
                has_unsaved_changes: false,
                modifiers: ModifiersState::empty(),
                save_message_until: None,
            };
            
            event_loop.run_app(&mut app).unwrap();
        }
        Err(e) => {
            eprintln!("Error creating board: {}", e);
        }
    }
}