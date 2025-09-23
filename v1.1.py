import cv2
import numpy as np
import pyautogui
import time
import os
import sys
import keyboard
from datetime import datetime
import tkinter as tk
from tkinter import ttk, messagebox, filedialog
from PIL import Image, ImageTk
import threading

class MasterworkerGUI:
    def __init__(self, root):
        self.root = root
        self.root.title("Diablo IV Masterworker")
        self.root.geometry("600x700")
        self.root.resizable(False, False)
        
        # Variables
        self.reference_images_gray = None
        self.is_running = False
        self.stop_program = False
        self.bot_thread = None
        
        # Capture variables
        self.capture_active = False
        self.capture_start_pos = None
        self.capture_overlay = None
        
        # Stats
        self.attempt_number = 0
        self.consecutive_successes = 0
        self.fail_count = 0
        
        self.setup_ui()
        self.load_initial_images()
        
    def setup_ui(self):
        # Main container
        main_frame = ttk.Frame(self.root, padding="10")
        main_frame.grid(row=0, column=0, sticky=(tk.W, tk.E, tk.N, tk.S))
        
        # Title
        title_label = ttk.Label(main_frame, text="Diablo IV Masterworker Bot", 
                               font=("Arial", 16, "bold"))
        title_label.grid(row=0, column=0, columnspan=2, pady=(0, 20))
        
        # Image setup section
        setup_frame = ttk.LabelFrame(main_frame, text="Image Setup", padding="10")
        setup_frame.grid(row=1, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        # Affix image section
        affix_frame = ttk.Frame(setup_frame)
        affix_frame.grid(row=0, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        # Top row: Label and status
        top_frame = ttk.Frame(affix_frame)
        top_frame.grid(row=0, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        ttk.Label(top_frame, text="Affix Detection Image:").grid(row=0, column=0, sticky=tk.W)
        
        self.affix_status_label = ttk.Label(top_frame, text="Not loaded", foreground="red")
        self.affix_status_label.grid(row=0, column=1, padx=(10, 0))
        
        # Middle row: Image preview
        preview_frame = ttk.Frame(affix_frame)
        preview_frame.grid(row=1, column=0, columnspan=2, pady=(0, 10))
        
        self.affix_preview_label = ttk.Label(preview_frame, text="No image loaded", 
                                           background="gray90", width=30, anchor="center")
        self.affix_preview_label.grid(row=0, column=0)
        
        # Bottom row: Buttons
        button_frame = ttk.Frame(affix_frame)
        button_frame.grid(row=2, column=0, columnspan=2, pady=(0, 0))
        
        ttk.Button(button_frame, text="Capture New Affix", 
                  command=self.capture_affix_simple).grid(row=0, column=0, padx=(0, 5))
        ttk.Button(button_frame, text="Advanced Capture", 
                  command=self.capture_affix_advanced).grid(row=0, column=1, padx=5)
        ttk.Button(button_frame, text="Load from File", 
                  command=self.load_affix_file).grid(row=0, column=2, padx=(5, 0))
        
        # Other images status
        ttk.Label(setup_frame, text="Other Required Images:").grid(row=1, column=0, sticky=tk.W, pady=(10, 0))
        self.images_status_label = ttk.Label(setup_frame, text="Checking...", foreground="orange")
        self.images_status_label.grid(row=1, column=1, padx=(10, 0), pady=(10, 0))
        
        # Settings section
        settings_frame = ttk.LabelFrame(main_frame, text="Settings", padding="10")
        settings_frame.grid(row=2, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        # Threshold settings
        ttk.Label(settings_frame, text="Affix Detection Threshold:").grid(row=0, column=0, sticky=tk.W)
        self.affix_threshold = tk.DoubleVar(value=0.8)
        affix_scale = ttk.Scale(settings_frame, from_=0.5, to=0.95, variable=self.affix_threshold, 
                               orient=tk.HORIZONTAL, length=200)
        affix_scale.grid(row=0, column=1, padx=(10, 5))
        self.affix_threshold_label = ttk.Label(settings_frame, text="0.80")
        self.affix_threshold_label.grid(row=0, column=2)
        affix_scale.configure(command=self.update_affix_threshold)
        
        ttk.Label(settings_frame, text="Other Elements Threshold:").grid(row=1, column=0, sticky=tk.W, pady=(5, 0))
        self.general_threshold = tk.DoubleVar(value=0.5)
        general_scale = ttk.Scale(settings_frame, from_=0.3, to=0.8, variable=self.general_threshold, 
                                 orient=tk.HORIZONTAL, length=200)
        general_scale.grid(row=1, column=1, padx=(10, 5), pady=(5, 0))
        self.general_threshold_label = ttk.Label(settings_frame, text="0.50")
        self.general_threshold_label.grid(row=1, column=2, pady=(5, 0))
        general_scale.configure(command=self.update_general_threshold)
        
        # Control section
        control_frame = ttk.LabelFrame(main_frame, text="Bot Control", padding="10")
        control_frame.grid(row=3, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        self.start_button = ttk.Button(control_frame, text="Start Bot", command=self.start_bot)
        self.start_button.grid(row=0, column=0, padx=(0, 10))
        
        self.stop_button = ttk.Button(control_frame, text="Stop Bot", command=self.stop_bot, state="disabled")
        self.stop_button.grid(row=0, column=1)
        
        # Status section
        status_frame = ttk.LabelFrame(main_frame, text="Status", padding="10")
        status_frame.grid(row=4, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        self.status_label = ttk.Label(status_frame, text="Ready to start", foreground="green")
        self.status_label.grid(row=0, column=0, columnspan=2)
        
        # Stats section
        stats_frame = ttk.LabelFrame(main_frame, text="Statistics", padding="10")
        stats_frame.grid(row=5, column=0, columnspan=2, sticky=(tk.W, tk.E), pady=(0, 10))
        
        self.attempts_label = ttk.Label(stats_frame, text="Attempts: 0")
        self.attempts_label.grid(row=0, column=0, sticky=tk.W)
        
        self.successes_label = ttk.Label(stats_frame, text="Consecutive Successes: 0/3")
        self.successes_label.grid(row=1, column=0, sticky=tk.W)
        
        self.fails_label = ttk.Label(stats_frame, text="Failures: 0")
        self.fails_label.grid(row=2, column=0, sticky=tk.W)
        
        # Log section
        log_frame = ttk.LabelFrame(main_frame, text="Log", padding="10")
        log_frame.grid(row=6, column=0, columnspan=2, sticky=(tk.W, tk.E, tk.N, tk.S), pady=(0, 10))
        
        # Text widget with scrollbar
        log_text_frame = ttk.Frame(log_frame)
        log_text_frame.grid(row=0, column=0, sticky=(tk.W, tk.E, tk.N, tk.S))
        
        self.log_text = tk.Text(log_text_frame, height=8, width=70, wrap=tk.WORD)
        scrollbar = ttk.Scrollbar(log_text_frame, orient=tk.VERTICAL, command=self.log_text.yview)
        self.log_text.configure(yscrollcommand=scrollbar.set)
        
        self.log_text.grid(row=0, column=0, sticky=(tk.W, tk.E, tk.N, tk.S))
        scrollbar.grid(row=0, column=1, sticky=(tk.N, tk.S))
        
        log_text_frame.grid_columnconfigure(0, weight=1)
        log_text_frame.grid_rowconfigure(0, weight=1)
        
        # Configure grid weights
        main_frame.grid_columnconfigure(0, weight=1)
        main_frame.grid_rowconfigure(6, weight=1)
        
    def resource_path(self, relative_path):
        base_path = getattr(sys, '_MEIPASS', os.path.dirname(os.path.abspath(__file__)))
        resources_dir = os.path.join(base_path, 'resources')
        if not os.path.exists(resources_dir):
            os.makedirs(resources_dir)
        return os.path.join(resources_dir, relative_path)
    
    def log_message(self, message, color=None):
        timestamp = datetime.now().strftime("%H:%M:%S")
        formatted_message = f"[{timestamp}] {message}\n"
        
        # Insert at the beginning (top) instead of end
        self.log_text.insert("1.0", formatted_message)
        
        # Apply color formatting if specified
        if color:
            # Get the line number of the inserted text
            line_start = "1.0"
            line_end = f"1.{len(formatted_message)-1}"  # -1 to exclude the newline
            
            # Create a tag for this color if it doesn't exist
            tag_name = f"color_{color}"
            if tag_name not in self.log_text.tag_names():
                self.log_text.tag_configure(tag_name, foreground=color)
            
            # Apply the color tag to the inserted line
            self.log_text.tag_add(tag_name, line_start, line_end)
        
        # Keep cursor at top
        self.log_text.see("1.0")
        self.root.update_idletasks()
    
    def update_affix_preview(self):
        """Update the affix image preview"""
        affix_path = self.resource_path("affix.png")
        
        if os.path.exists(affix_path):
            try:
                # Load and resize image for preview
                img = Image.open(affix_path)
                
                # Calculate size maintaining aspect ratio (max 200x100)
                original_width, original_height = img.size
                max_width, max_height = 200, 100
                
                ratio = min(max_width/original_width, max_height/original_height)
                new_width = int(original_width * ratio)
                new_height = int(original_height * ratio)
                
                # Resize image
                img_resized = img.resize((new_width, new_height), Image.Resampling.LANCZOS)
                
                # Convert to PhotoImage
                self.affix_preview_photo = ImageTk.PhotoImage(img_resized)
                
                # Update label
                self.affix_preview_label.configure(
                    image=self.affix_preview_photo, 
                    text="",
                    compound=tk.CENTER
                )
                
                # Update status with image dimensions
                self.affix_status_label.config(
                    text=f"Loaded ({original_width}x{original_height}px)", 
                    foreground="green"
                )
                
            except Exception as e:
                self.affix_preview_label.configure(
                    image="", 
                    text=f"Error loading preview: {str(e)[:30]}...",
                    compound=tk.CENTER
                )
                self.affix_status_label.config(text="Error loading", foreground="red")
        else:
            # No image found
            self.affix_preview_label.configure(
                image="", 
                text="No affix image found\nClick a button below to add one",
                compound=tk.CENTER
            )
            self.affix_status_label.config(text="Not found", foreground="red")
    
    def update_affix_threshold(self, value):
        self.affix_threshold_label.config(text=f"{float(value):.2f}")
    
    def update_general_threshold(self, value):
        self.general_threshold_label.config(text=f"{float(value):.2f}")
    
    def update_stats(self):
        self.attempts_label.config(text=f"Attempts: {self.attempt_number}")
        self.successes_label.config(text=f"Consecutive Successes: {self.consecutive_successes}/3")
        self.fails_label.config(text=f"Failures: {self.fail_count}")
    
    def capture_affix_simple(self):
        """Simple capture using mouse clicks instead of keyboard shortcuts"""
        self.log_message("Starting simple affix capture...")
        self.log_message("Click at TOP-LEFT corner of the affix area")
        
        def capture_thread():
            try:
                self.capture_active = True
                self.capture_start_pos = None
                
                # Create a transparent overlay window to capture clicks
                self.create_capture_overlay()
                
                # Wait for the capture to complete
                while self.capture_active and not self.stop_program:
                    time.sleep(0.1)
                
                if self.capture_overlay:
                    self.capture_overlay.destroy()
                    self.capture_overlay = None
                
            except Exception as e:
                self.log_message(f"Capture error: {e}")
                if self.capture_overlay:
                    self.capture_overlay.destroy()
                    self.capture_overlay = None
        
        threading.Thread(target=capture_thread, daemon=True).start()
    
    def create_capture_overlay(self):
        """Create a transparent overlay for capturing mouse clicks"""
        self.capture_overlay = tk.Toplevel(self.root)
        self.capture_overlay.attributes('-fullscreen', True)
        self.capture_overlay.attributes('-alpha', 0.1)  # Very transparent
        self.capture_overlay.attributes('-topmost', True)
        self.capture_overlay.configure(bg='black')
        
        # Create canvas for instructions
        canvas = tk.Canvas(self.capture_overlay, highlightthickness=0, bg='black')
        canvas.pack(fill=tk.BOTH, expand=True)
        
        # Add instruction text
        screen_width = self.capture_overlay.winfo_screenwidth()
        screen_height = self.capture_overlay.winfo_screenheight()
        
        if self.capture_start_pos is None:
            instruction = "Click at TOP-LEFT corner of affix area\nPress ESC to cancel"
        else:
            instruction = "Click at BOTTOM-RIGHT corner of affix area\nPress ESC to cancel"
        
        canvas.create_text(screen_width // 2, 50, 
                          text=instruction,
                          fill='yellow', font=('Arial', 16, 'bold'))
        
        # Bind events
        self.capture_overlay.bind("<Button-1>", self.on_capture_click)
        self.capture_overlay.bind("<Escape>", self.cancel_capture)
        self.capture_overlay.focus_set()
    
    def on_capture_click(self, event):
        """Handle mouse clicks during capture"""
        try:
            # Get absolute mouse position
            mouse_x = self.capture_overlay.winfo_pointerx()
            mouse_y = self.capture_overlay.winfo_pointery()
            
            if self.capture_start_pos is None:
                # First click - store start position
                self.capture_start_pos = (mouse_x, mouse_y)
                self.log_message(f"Start position set: {self.capture_start_pos}")
                
                # Update overlay for second click
                self.capture_overlay.destroy()
                self.create_capture_overlay()
                
            else:
                # Second click - capture the area
                end_pos = (mouse_x, mouse_y)
                self.log_message(f"End position set: {end_pos}")
                
                # Calculate region
                x1 = min(self.capture_start_pos[0], end_pos[0])
                y1 = min(self.capture_start_pos[1], end_pos[1])
                x2 = max(self.capture_start_pos[0], end_pos[0])
                y2 = max(self.capture_start_pos[1], end_pos[1])
                
                width = x2 - x1
                height = y2 - y1
                
                if width < 10 or height < 10:
                    self.log_message("Selected area too small! Try again.")
                    self.capture_start_pos = None
                    self.capture_overlay.destroy()
                    self.create_capture_overlay()
                    return
                
                # Take screenshot of the selected region
                screenshot = pyautogui.screenshot(region=(x1, y1, width, height))
                affix_path = self.resource_path("affix.png")
                screenshot.save(affix_path)
                
                self.log_message(f"Affix screenshot saved! Size: {width}x{height}")
                
                # Complete the capture
                self.capture_active = False
                self.load_initial_images()
                self.update_affix_preview()
                
        except Exception as e:
            self.log_message(f"Error during capture: {e}")
            self.cancel_capture(None)
    
    def cancel_capture(self, event):
        """Cancel the capture process"""
        self.capture_active = False
        self.capture_start_pos = None
        self.log_message("Capture cancelled")
        if self.capture_overlay:
            self.capture_overlay.destroy()
            self.capture_overlay = None
    
    def capture_affix_advanced(self):
        self.log_message("Starting advanced capture - Click and drag to select area")
        
        def capture_thread():
            try:
                self.log_message("Advanced capture window opening...")
                
                screenshot = pyautogui.screenshot()
                
                # Create capture window
                capture_window = tk.Toplevel(self.root)
                capture_window.attributes('-fullscreen', True)
                capture_window.attributes('-alpha', 0.3)
                capture_window.attributes('-topmost', True)
                capture_window.configure(bg='black')
                
                photo = ImageTk.PhotoImage(screenshot)
                canvas = tk.Canvas(capture_window, highlightthickness=0)
                canvas.pack(fill=tk.BOTH, expand=True)
                canvas.create_image(0, 0, anchor=tk.NW, image=photo)
                
                start_x = start_y = end_x = end_y = 0
                rect_id = None
                success = False
                
                def on_mouse_press(event):
                    nonlocal start_x, start_y, rect_id
                    start_x, start_y = event.x, event.y
                    if rect_id:
                        canvas.delete(rect_id)
                
                def on_mouse_drag(event):
                    nonlocal rect_id, end_x, end_y
                    end_x, end_y = event.x, event.y
                    if rect_id:
                        canvas.delete(rect_id)
                    rect_id = canvas.create_rectangle(start_x, start_y, end_x, end_y, 
                                                    outline='red', width=3, fill='')
                
                def on_mouse_release(event):
                    nonlocal success
                    end_x, end_y = event.x, event.y
                    
                    x1, y1 = min(start_x, end_x), min(start_y, end_y)
                    x2, y2 = max(start_x, end_x), max(start_y, end_y)
                    
                    if (x2 - x1) < 10 or (y2 - y1) < 10:
                        self.log_message("Selected area too small!")
                        return
                    
                    cropped = screenshot.crop((x1, y1, x2, y2))
                    affix_path = self.resource_path("affix.png")
                    cropped.save(affix_path)
                    
                    self.log_message(f"Advanced capture successful! Size: {x2-x1}x{y2-y1}")
                    success = True
                    capture_window.destroy()
                    self.load_initial_images()
                    self.update_affix_preview()
                
                def on_escape(event):
                    capture_window.destroy()
                    self.log_message("Advanced capture cancelled")
                
                canvas.bind("<Button-1>", on_mouse_press)
                canvas.bind("<B1-Motion>", on_mouse_drag)
                canvas.bind("<ButtonRelease-1>", on_mouse_release)
                capture_window.bind("<Escape>", on_escape)
                
                canvas.create_text(canvas.winfo_screenwidth()//2, 50, 
                                 text="Click and drag to select affix area. Press ESC to cancel.",
                                 fill='yellow', font=('Arial', 16, 'bold'))
                
                capture_window.focus_set()
                
            except Exception as e:
                self.log_message(f"Advanced capture error: {e}")
        
        threading.Thread(target=capture_thread, daemon=True).start()
    
    def load_affix_file(self):
        file_path = filedialog.askopenfilename(
            title="Select Affix Image",
            filetypes=[("PNG files", "*.png"), ("Image files", "*.png *.jpg *.jpeg")]
        )
        
        if file_path:
            try:
                # Copy file to resources directory
                affix_path = self.resource_path("affix.png")
                img = Image.open(file_path)
                img.save(affix_path)
                self.log_message(f"Affix image loaded from: {file_path}")
                self.load_initial_images()
                self.update_affix_preview()
            except Exception as e:
                self.log_message(f"Error loading file: {e}")
                messagebox.showerror("Error", f"Could not load image: {e}")
    
    def load_initial_images(self):
        try:
            reference_images = [
                self.resource_path("upgrade_1.png"),
                self.resource_path("upgrade_2.png"), 
                self.resource_path("upgrade_3.png"),
                self.resource_path("skip.png"),
                self.resource_path("affix.png"),
                self.resource_path("reset.png"),
                self.resource_path("confirm.png")
            ]
            
            # Update affix preview first
            self.update_affix_preview()
            
            # Check other images
            missing_images = []
            self.reference_images_gray = []
            
            for i, img_path in enumerate(reference_images):
                if os.path.exists(img_path):
                    img = cv2.imread(img_path, 0)
                    if img is not None:
                        self.reference_images_gray.append(img)
                    else:
                        missing_images.append(os.path.basename(img_path))
                else:
                    missing_images.append(os.path.basename(img_path))
            
            if missing_images:
                self.images_status_label.config(text=f"Missing: {', '.join(missing_images)}", foreground="red")
            else:
                self.images_status_label.config(text="All loaded", foreground="green")
                
        except Exception as e:
            self.log_message(f"Error loading images: {e}")
    
    def start_bot(self):
        if not self.reference_images_gray or len(self.reference_images_gray) != 7:
            messagebox.showerror("Error", "Please ensure all required images are loaded first!")
            return
        
        self.is_running = True
        self.stop_program = False
        self.start_button.config(state="disabled")
        self.stop_button.config(state="normal")
        self.status_label.config(text="Bot running...", foreground="blue")
        
        # Reset stats
        self.attempt_number = 0
        self.consecutive_successes = 0
        self.fail_count = 0
        self.update_stats()
        
        self.log_message("Bot started!")
        
        self.bot_thread = threading.Thread(target=self.run_bot, daemon=True)
        self.bot_thread.start()
    
    def stop_bot(self):
        self.stop_program = True
        self.is_running = False
        self.start_button.config(state="normal")
        self.stop_button.config(state="disabled")
        self.status_label.config(text="Bot stopped", foreground="red")
        self.log_message("Bot stopped by user")
    
    def run_bot(self):
        skip_not_found_count = 0
        upgrade_not_found_count = 0
        
        while not self.stop_program and self.consecutive_successes < 3:
            try:
                # Check if Diablo window is active
                window = pyautogui.getActiveWindow()
                if window is None or "Diablo IV" not in window.title:
                    self.log_message("Diablo IV window not focused - waiting...")
                    time.sleep(2)
                    continue
                
                success, upgrade_not_found_count = self.process_upgrade_attempt(upgrade_not_found_count)
                
                if success is None:  # Upgrade button not found
                    if upgrade_not_found_count > 5:
                        self.log_message("Upgrade button not found too many times - stopping")
                        break
                    continue
                
                self.attempt_number += 1
                
                if success:
                    self.consecutive_successes += 1
                    self.log_message(f"Attempt {self.attempt_number} -> SUCCESS [{self.consecutive_successes}/3]", "green")
                    skip_not_found_count = 0
                else:
                    if success is False:  # Skip button not found
                        skip_not_found_count += 1
                        if skip_not_found_count >= 2:
                            self.log_message("Not enough materials - stopping", "red")
                            break
                    else:  # Failed upgrade
                        self.fail_count += 1
                        self.consecutive_successes = 0
                        self.log_message(f"Attempt {self.attempt_number} -> FAILED [{self.fail_count}]", "red")
                        skip_not_found_count = 0
                
                self.update_stats()
                time.sleep(0.5)
                
            except Exception as e:
                self.log_message(f"Bot error: {e}")
                break
        
        # Final status
        if self.consecutive_successes == 3:
            self.status_label.config(text="Item fully masterworked!", foreground="green")
            self.log_message(f"SUCCESS! Item fully masterworked after {self.attempt_number} attempts!", "green")
        else:
            self.status_label.config(text="Bot stopped", foreground="red")
        
        self.is_running = False
        self.start_button.config(state="normal")
        self.stop_button.config(state="disabled")
    
    def process_upgrade_attempt(self, upgrade_not_found_count):
        # Move cursor to center
        screen_width, screen_height = pyautogui.size()
        pyautogui.moveTo(screen_width // 2, screen_height // 2)
        
        # Find upgrade button
        upgrade_pos = self.find_image_on_screen(self.reference_images_gray[0], self.general_threshold.get())
        if not upgrade_pos:
            upgrade_not_found_count += 1
            return None, upgrade_not_found_count
        
        # Click upgrade button
        pyautogui.moveTo(*upgrade_pos)
        for _ in range(4):
            pyautogui.click()
            time.sleep(0.1)
        time.sleep(0.1)
        
        # Find and click skip
        skip_pos = self.find_image_on_screen(self.reference_images_gray[3], self.general_threshold.get())
        if not skip_pos:
            return False, upgrade_not_found_count  # Skip not found = no materials
        
        pyautogui.moveTo(*skip_pos)
        pyautogui.click()
        time.sleep(1.25)
        
        # Check for affix (success)
        affix_pos = self.find_image_on_screen(self.reference_images_gray[4], self.affix_threshold.get())
        if affix_pos:
            pyautogui.click()
            return True, upgrade_not_found_count
        
        # Failed - reset
        pyautogui.click()
        time.sleep(0.1)
        
        # Find and click reset
        reset_pos = self.find_image_on_screen(self.reference_images_gray[5], self.general_threshold.get())
        if reset_pos:
            pyautogui.moveTo(*reset_pos)
            pyautogui.click()
            time.sleep(0.1)
            
            # Find and click confirm
            confirm_pos = self.find_image_on_screen(self.reference_images_gray[6], self.general_threshold.get())
            if confirm_pos:
                pyautogui.moveTo(*confirm_pos)
                pyautogui.click()
                time.sleep(0.1)
        
        return 0, upgrade_not_found_count  # Failed attempt
    
    def find_image_on_screen(self, reference_image_gray, threshold):
        screenshot = np.array(pyautogui.screenshot())
        screenshot_gray = cv2.cvtColor(screenshot, cv2.COLOR_RGB2GRAY)
        res = cv2.matchTemplate(screenshot_gray, reference_image_gray, cv2.TM_CCOEFF_NORMED)
        min_val, max_val, min_loc, max_loc = cv2.minMaxLoc(res)
        
        # Debug logging for affix detection
        if len(self.reference_images_gray) > 4 and np.array_equal(reference_image_gray, self.reference_images_gray[4]):
            self.log_message(f"Affix detection confidence: {max_val:.3f} (threshold: {threshold:.3f})")
        
        if max_val >= threshold:
            return (max_loc[0] + reference_image_gray.shape[1] // 2,
                    max_loc[1] + reference_image_gray.shape[0] // 2)
        return None

def main():
    root = tk.Tk()
    app = MasterworkerGUI(root)
    root.mainloop()

if __name__ == "__main__":
    main()