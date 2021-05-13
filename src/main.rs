use std::{
    fs::File,
    io,
    io::Write,
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::atomic::AtomicBool,
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
    os::unix::io::AsRawFd
};

use v4l2r::{decoder::stateful::GetBufferError, device::queue::handles_provider::MmapProvider};
use v4l2r::{decoder::FormatChangedReply, device::queue::FormatBuilder, memory::MemoryType};
use v4l2r::{
    decoder::{stateful::Decoder, DecoderEvent},
    device::{
        poller::PollError,
        queue::{direction::Capture, dqbuf::DqBuffer},
    },
    memory::MmapHandle,
    Format, Rect,
};

use aho_corasick::AhoCorasick;

const DEVICE_PATH: &'static str = "/dev/video10";
const VIDEO_FILE_PATH: &'static str = "FPS_test_1080p60_L4.2.h264";

fn main() {
    let mut stream =
        BufReader::new(File::open(VIDEO_FILE_PATH).expect("Compressed stream not found"));
    let decoder_file = File::open(&DEVICE_PATH).expect("Unable to open decoder");

    let lets_quit = Arc::new(AtomicBool::new(false));
    // Setup the Ctrl+c handler.
    {
        let lets_quit_handler = lets_quit.clone();
        ctrlc::set_handler(move || {
            lets_quit_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl-C handler.");
    }

    const NUM_OUTPUT_BUFFERS: usize = 4;

    let poll_count_reader = Arc::new(AtomicUsize::new(0));
    let poll_count_writer = Arc::clone(&poll_count_reader);
    let start_time = std::time::Instant::now();
    let mut output_buffer_size = 0usize;
    let mut frame_counter = 0usize;
    let mut output_ready_cb = move |cap_dqbuf: DqBuffer<Capture, Vec<MmapHandle>>| {
        let bytes_used = cap_dqbuf.data.get_first_plane().bytesused() as usize;
        // Ignore zero-sized buffers.
        if bytes_used == 0 {
            return;
        }

        let elapsed = start_time.elapsed();
        frame_counter += 1;
        let fps = frame_counter as f32 / elapsed.as_millis() as f32 * 1000.0;
        let ppf = poll_count_reader.load(Ordering::SeqCst) as f32 / frame_counter as f32;
        print!(
            "\rDecoded buffer {:#5}, index: {:#2}), bytes used:{:#6} fps: {:#5.2} ppf: {:#4.2}",
            cap_dqbuf.data.sequence(),
            cap_dqbuf.data.index(),
            bytes_used,
            fps,
            ppf,
        );
        io::stdout().flush().unwrap();
    };
    let decoder_event_cb = move |event: DecoderEvent<MmapProvider>| match event {
        DecoderEvent::FrameDecoded(dqbuf) => output_ready_cb(dqbuf),
        DecoderEvent::EndOfStream => (),
    };
    let set_capture_format_cb = move |f: FormatBuilder,
                                      visible_rect: Rect,
                                      min_num_buffers: usize|
          -> anyhow::Result<FormatChangedReply<MmapProvider>> {
        let format = f.set_pixelformat(b"RGBP").apply()?;

        println!(
            "New CAPTURE format: {:?} (visible rect: {})",
            format, visible_rect
        );

        let provider = MmapProvider::new(&format);

        Ok(FormatChangedReply {
            provider,
            // TODO: can't the provider report the memory type that it is
            // actually serving itself?
            mem_type: MemoryType::Mmap,
            num_buffers: min_num_buffers,
        })
    };

    
    let mut decoder = Decoder::open(&Path::new(&DEVICE_PATH))
    .expect("Failed to open device")
    .set_output_format(|f| {
        let format: Format = f.set_pixelformat(b"H264").apply()?;
        
        assert_eq!(
            format.pixelformat,
            b"H264".into(),
            "H264 format not supported"
        );
        
        println!("Temporary output format: {:?}", format);
        
        output_buffer_size = format.plane_fmt[0].sizeimage as usize;
        
        Ok(())
    })
    .expect("Failed to set output format")
    .allocate_output_buffers::<Vec<MmapHandle>>(NUM_OUTPUT_BUFFERS)
    .expect("Failed to allocate output buffers")
    .set_poll_counter(poll_count_writer)
    .start(|_| (), decoder_event_cb, set_capture_format_cb)
    .expect("Failed to start decoder");
    
    // Remove mutability.
    let output_buffer_size = output_buffer_size;
    
    println!("Allocated {} buffers", decoder.num_output_buffers());
    println!("Required size for output buffers: {}", output_buffer_size);

    // This shouldn't be needed, this is an RPI issue
    v4l2r::ioctl::streamon(&decoder_file.as_raw_fd(), v4l2r::QueueType::VideoCaptureMplane).expect("Unable to start capture queue");
    
    let mut total_size: usize = 0;
    
    while !lets_quit.load(Ordering::SeqCst) {
        
        let v4l2_buffer = match decoder.get_buffer() {
            Ok(buffer) => buffer,
            // If we got interrupted while waiting for a buffer, just exit normally.
            Err(GetBufferError::PollError(PollError::EPollWait(e)))
            if e.kind() == io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(e) => {
                panic!("{}", e);
            },
        };

        let mut mapping = v4l2_buffer
            .get_plane_mapping(0)
            .expect("Failed to get Mmap mapping");

        let bytes_used = read_next_aud(&mut stream, &mut mapping.data)
            .expect("Unable to load data into mmap handle");

        v4l2_buffer
            .queue(&[bytes_used])
            .expect("Failed to queue output buffer");

        total_size += bytes_used;
    }

    decoder.drain(true).unwrap();
    decoder.stop().unwrap();
    println!();

    println!("Total size: {}", total_size);
}

// Read more data from the file into dst until we run into the pattern [0,0,0,1]
fn read_next_aud(file: &mut BufReader<File>, dst: &mut [u8]) -> anyhow::Result<usize> {
    let mut bytes_used = 0;

    let h264_aud_pattern: AhoCorasick = AhoCorasick::new_auto_configured(&[[0, 0, 0, 1]]);

    loop {
        let buffer = file.fill_buf()?;

        // Check if buffer is empty right after filling it. If it is it means we've hit the end of the file.
        if buffer.is_empty() {
            return Err(anyhow::anyhow!("End of file"));
        }

        // Search for the
        match h264_aud_pattern.find(&buffer[1..]) {
            Some(mat) => {
                let dst_end = bytes_used + mat.start() + 1;
                // println!("Read from {:x} to {:x}", bytes_used, dst_end);
                file.read_exact(&mut dst[bytes_used..dst_end])?;
                bytes_used += mat.start() + 1;
                break;
            }
            None => {
                let buffer_len = buffer.len();
                let dst_end = bytes_used + buffer_len;
                file.read_exact(&mut dst[bytes_used..dst_end])?;
                bytes_used += buffer_len;
            }
        }
    }

    let buffer = file.fill_buf()?;
    // The buffer we have filled should start with the aud pattern
    debug_assert_eq!(&dst[..4], &[0, 0, 0, 1]);

    // The buffer we will create next should start with the aud pattern
    if !buffer.is_empty() {
        debug_assert_eq!(&buffer[..4], &[0, 0, 0, 1]);
    }

    Ok(bytes_used)
}
