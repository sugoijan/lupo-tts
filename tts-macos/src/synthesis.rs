// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioCommonFormat, AVAudioConverter, AVAudioFormat, AVAudioPCMBuffer,
    AVSpeechSynthesisVoice, AVSpeechSynthesizer, AVSpeechUtterance,
};
use objc2_foundation::{NSDate, NSError, NSRunLoop, NSString};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Synthesize speech using macOS AVSpeech APIs on the current main thread.
pub fn synthesize_on_main_thread(message: &str) -> Result<Vec<u8>> {
    synthesize(message.to_owned())
}

fn synthesize(message: String) -> Result<Vec<u8>> {
    autoreleasepool(|_| unsafe {
        // AVSpeechSynthesizer write callbacks require main-thread run loop coordination.
        MainThreadMarker::new().context("macOS system TTS must run on the main thread")?;

        let synthesizer = AVSpeechSynthesizer::new();
        synthesizer.setUsesApplicationAudioSession(false);

        let ns_message = NSString::from_str(&message);
        let utterance = AVSpeechUtterance::speechUtteranceWithString(&ns_message);
        if let Some(language_voice) =
            AVSpeechSynthesisVoice::voiceWithLanguage(Some(&NSString::from_str("en-US")))
        {
            utterance.setVoice(Some(&language_voice));
        }

        let state = Rc::new(RefCell::new(Collector::default()));
        let state_for_block = Rc::clone(&state);

        let block = RcBlock::new(move |buffer_ptr: NonNull<AVAudioBuffer>| {
            let mut guard = state_for_block.borrow_mut();
            if guard.error.is_some() {
                return;
            }
            if let Err(err) = guard.consume_buffer(buffer_ptr) {
                guard.error = Some(err);
            }
        });

        let raw_block = RcBlock::into_raw(block);
        synthesizer.writeUtterance_toBufferCallback(&utterance, raw_block);
        let block_handle = RcBlock::from_raw(raw_block);

        let run_loop = NSRunLoop::currentRunLoop();
        let deadline = Instant::now() + Duration::from_secs(15);

        loop {
            {
                let guard = state.borrow();
                if guard.error.is_some() {
                    break;
                }
                if guard.had_samples && !synthesizer.isSpeaking() {
                    break;
                }
            }

            if Instant::now() >= deadline {
                let mut guard = state.borrow_mut();
                guard.error = Some(anyhow!("speech synthesizer timed out"));
                break;
            }

            let next_step = NSDate::dateWithTimeIntervalSinceNow(0.05);
            run_loop.runUntilDate(&next_step);
        }

        drop(block_handle);

        let mut guard = state.borrow_mut();
        if let Some(err) = guard.error.take() {
            return Err(err);
        }

        guard.finalize()
    })
}

#[derive(Default)]
struct Collector {
    source_format: Option<Retained<AVAudioFormat>>,
    sample_rate: Option<f64>,
    channels: Option<u16>,
    common_format: Option<AVAudioCommonFormat>,
    buffers: Vec<Retained<AVAudioPCMBuffer>>,
    total_frames: usize,
    had_samples: bool,
    error: Option<anyhow::Error>,
}

impl Collector {
    fn consume_buffer(&mut self, buffer_ptr: NonNull<AVAudioBuffer>) -> Result<()> {
        unsafe {
            let pcm_buffer = buffer_ptr.cast::<AVAudioPCMBuffer>().as_ref();
            let frame_length = pcm_buffer.frameLength() as usize;
            if frame_length == 0 {
                return Ok(());
            }

            let format = pcm_buffer.format();
            let sample_rate = format.sampleRate();
            let channel_count = format.channelCount();
            let common_format = format.commonFormat();

            self.verify_sample_rate(sample_rate)?;
            self.verify_channels(channel_count)?;
            self.verify_common_format(common_format)?;

            if self.source_format.is_none() {
                self.source_format = Some(format);
            }

            let retained_buffer = Retained::retain(pcm_buffer as *const _ as *mut AVAudioPCMBuffer)
                .context("failed to retain speech synthesizer buffer")?;
            self.buffers.push(retained_buffer);
            self.total_frames += frame_length;
            self.had_samples = true;
        }

        Ok(())
    }

    fn finalize(&mut self) -> Result<Vec<u8>> {
        if self.buffers.is_empty() {
            bail!("speech synthesizer did not produce any audio buffers");
        }

        let sample_rate = self
            .sample_rate
            .context("missing sample rate information from speech synthesizer")?;
        let sample_rate_u32 = normalize_sample_rate(sample_rate)?;
        let channels = self
            .channels
            .context("missing channel information from speech synthesizer")?;

        let source_format = self
            .source_format
            .as_ref()
            .context("missing source format from speech synthesizer")?;

        let destination_format = create_destination_format(sample_rate, channels)?;
        let converter = create_converter(source_format.as_ref(), destination_format.as_ref())?;

        let mut buffers = std::mem::take(&mut self.buffers);
        let mut samples = Vec::<i16>::with_capacity(self.total_frames * channels as usize);

        for buffer in buffers.drain(..) {
            convert_buffer(
                converter.as_ref(),
                destination_format.as_ref(),
                buffer.as_ref(),
                channels,
                &mut samples,
            )?;
        }

        Ok(write_wav(&samples, channels, sample_rate_u32))
    }

    fn verify_sample_rate(&mut self, sample_rate: f64) -> Result<()> {
        match self.sample_rate {
            Some(existing) if (existing - sample_rate).abs() > f64::EPSILON => {
                bail!("speech synthesizer changed sample rate mid-stream")
            }
            None => self.sample_rate = Some(sample_rate),
            _ => {}
        }
        Ok(())
    }

    fn verify_channels(&mut self, channel_count: u32) -> Result<()> {
        let channel_u16 = u16::try_from(channel_count)
            .context("speech synthesizer reported channel count outside u16 range")?;
        match self.channels {
            Some(existing) if existing != channel_u16 => {
                bail!("speech synthesizer changed channel count mid-stream")
            }
            None => self.channels = Some(channel_u16),
            _ => {}
        }
        Ok(())
    }

    fn verify_common_format(&mut self, format: AVAudioCommonFormat) -> Result<()> {
        match self.common_format {
            Some(existing) if existing != format => {
                bail!("speech synthesizer changed PCM format mid-stream")
            }
            None => self.common_format = Some(format),
            _ => {}
        }
        Ok(())
    }
}

fn normalize_sample_rate(sample_rate: f64) -> Result<u32> {
    let rounded = sample_rate.round();
    if (sample_rate - rounded).abs() > 1e-6 {
        bail!("unsupported non-integer sample rate: {sample_rate}");
    }
    Ok(rounded as u32)
}

fn create_destination_format(sample_rate: f64, channels: u16) -> Result<Retained<AVAudioFormat>> {
    let format = unsafe {
        AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
            AVAudioFormat::alloc(),
            AVAudioCommonFormat::PCMFormatInt16,
            sample_rate,
            u32::from(channels),
            false,
        )
    }
    .context("failed to create destination audio format")?;
    Ok(format)
}

fn create_converter(
    source: &AVAudioFormat,
    destination: &AVAudioFormat,
) -> Result<Retained<AVAudioConverter>> {
    unsafe {
        AVAudioConverter::initFromFormat_toFormat(AVAudioConverter::alloc(), source, destination)
    }
    .context("failed to initialise AVAudioConverter")
}

fn convert_buffer(
    converter: &AVAudioConverter,
    destination_format: &AVAudioFormat,
    input_buffer: &AVAudioPCMBuffer,
    channels: u16,
    output: &mut Vec<i16>,
) -> Result<()> {
    unsafe {
        let frame_length = input_buffer.frameLength();
        let output_buffer = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
            AVAudioPCMBuffer::alloc(),
            destination_format,
            frame_length,
        )
        .context("failed to allocate destination audio buffer")?;
        output_buffer.setFrameLength(frame_length);

        converter
            .convertToBuffer_fromBuffer_error(output_buffer.as_ref(), input_buffer)
            .map_err(|error| anyhow!(ns_error_to_string(&error)))?;

        let data = output_buffer.int16ChannelData();
        if data.is_null() {
            bail!("converter returned buffer without int16 channel data");
        }

        let channel_ptrs =
            std::slice::from_raw_parts(data as *const NonNull<i16>, channels as usize);

        for frame in 0..frame_length as usize {
            for channel_ptr in channel_ptrs.iter() {
                let sample_ptr = channel_ptr.as_ptr().add(frame);
                output.push(*sample_ptr);
            }
        }
    }

    Ok(())
}

fn ns_error_to_string(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

fn write_wav(samples: &[i16], channels: u16, sample_rate: u32) -> Vec<u8> {
    const BITS_PER_SAMPLE: u16 = 16;

    let data_size = std::mem::size_of_val(samples) as u32;
    let block_align = channels * (BITS_PER_SAMPLE / 8);
    let byte_rate = sample_rate * block_align as u32;
    let chunk_size = 36 + data_size;

    let mut bytes = Vec::with_capacity(44 + data_size as usize);
    bytes.extend_from_slice(b"RIFF");
    write_u32_le(&mut bytes, chunk_size);
    bytes.extend_from_slice(b"WAVEfmt ");
    write_u32_le(&mut bytes, 16); // PCM chunk size
    write_u16_le(&mut bytes, 1); // PCM format
    write_u16_le(&mut bytes, channels);
    write_u32_le(&mut bytes, sample_rate);
    write_u32_le(&mut bytes, byte_rate);
    write_u16_le(&mut bytes, block_align);
    write_u16_le(&mut bytes, BITS_PER_SAMPLE);
    bytes.extend_from_slice(b"data");
    write_u32_le(&mut bytes, data_size);

    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    bytes
}

fn write_u16_le(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn write_u32_le(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}
