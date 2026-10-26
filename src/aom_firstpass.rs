pub mod aom {
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    use tokio::io::{AsyncRead, AsyncReadExt, Error};

    /// Record layout used by the aomenc that produced the stats log.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum FirstpassFormat {
        /// libaom before v3.7: 27 fields, no log_* or lt_coded_error.
        V3_6,
        /// libaom v3.7 through v3.13: 29 fields, adds log_intra_error and log_coded_error.
        V3_7,
        /// libaom v3.14 and later: 30 fields, adds lt_coded_error.
        V3_14,
    }

    impl FirstpassFormat {
        pub fn record_size(self) -> usize {
            match self {
                FirstpassFormat::V3_6 => 27 * 8,
                FirstpassFormat::V3_7 => 29 * 8,
                FirstpassFormat::V3_14 => 30 * 8,
            }
        }

        pub fn has_lt_coded_error(self) -> bool {
            self == FirstpassFormat::V3_14
        }

        fn decode(self, record: &[u8; 30 * 8]) -> AomFirstpass {
            let mut bytes = [0u8; 30 * 8];
            bytes[..self.record_size()].copy_from_slice(&record[..self.record_size()]);
            if !self.has_lt_coded_error() {
                bytes.copy_within(6 * 8..self.record_size(), 7 * 8);
                bytes[6 * 8..7 * 8].fill(0);
            }
            unsafe { std::mem::transmute(bytes) }
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct AomFirstpass {
        /*
         * Frame number in display order, if stats are for a single frame.
         * No real meaning for a collection of frames.
         */
        pub frame: f64,
        /*
         * Weight assigned to this frame (or total weight for the collection of
         * frames) currently based on intra factor and brightness factor. This is used
         * to distribute bits betweeen easier and harder frames.
         */
        weight: f64,
        /*
         * Intra prediction error.
         */
        intra_error: f64,
        /*
         * Average wavelet energy computed using Discrete Wavelet Transform (DWT).
         */
        frame_avg_wavelet_energy: f64,
        /*
         * Best of intra pred error and inter pred error using last frame as ref.
         */
        coded_error: f64,
        /*
         * Best of intra pred error and inter pred error using golden frame as ref.
         */
        sr_coded_error: f64,
        /*
         * Best of intra pred error and inter pred error using long term frame as ref.
         */
        lt_coded_error: f64,
        /*
         * Percentage of blocks with inter pred error < intra pred error.
         */
        pcnt_inter: f64,
        /*
         * Percentage of blocks using (inter prediction and) non-zero motion vectors.
         */
        pcnt_motion: f64,
        /*
         * Percentage of blocks where golden frame was better than last or intra:
         * inter pred error using golden frame < inter pred error using last frame and
         * inter pred error using golden frame < intra pred error
         */
        pcnt_second_ref: f64,
        /*
         * Percentage of blocks where intra and inter prediction errors were very
         * close. Note that this is a 'weighted count', that is, the so blocks may be
         * weighted by how close the two errors were.
         */
        pcnt_neutral: f64,
        /*
         * Percentage of blocks that have almost no intra error residual
         * (i.e. are in effect completely flat and untextured in the intra
         * domain). In natural videos this is uncommon, but it is much more
         * common in animations, graphics and screen content, so may be used
         * as a signal to detect these types of content.
         */
        intra_skip_pct: f64,
        /*
         * Image mask rows top and bottom.
         */
        inactive_zone_rows: f64,
        /*
         * Image mask columns at left and right edges.
         */
        inactive_zone_cols: f64,
        /*
         * Average of row motion vectors.
         */
        MVr: f64,
        /*
         * Mean of absolute value of row motion vectors.
         */
        mvr_abs: f64,
        /*
         * Mean of column motion vectors.
         */
        MVc: f64,
        /*
         * Mean of absolute value of column motion vectors.
         */
        mvc_abs: f64,
        /*
         * Variance of row motion vectors.
         */
        MVrv: f64,
        /*
         * Variance of column motion vectors.
         */
        MVcv: f64,
        /*
         * Value in range [-1,1] indicating fraction of row and column motion vectors
         * that point inwards (negative MV value) or outwards (positive MV value).
         * For example, value of 1 indicates, all row/column MVs are inwards.
         */
        mv_in_out_count: f64,
        /*
         * Count of unique non-zero motion vectors.
         */
        new_mv_count: f64,
        /*
         * Duration of the frame / collection of frames.
         */
        duration: f64,
        /*
         * 1.0 if stats are for a single frame, OR
         * Number of frames in this collection for which the stats are accumulated.
         */
        count: f64,
        /*
         * standard deviation for (0, 0) motion prediction error
         */
        raw_error_stdev: f64,
        /*
         * Whether the frame contains a flash
         */
        is_flash: i64,
        /*
         * Estimated noise variance
         */
        noise_var: f64,
        /*
         * Correlation coefficient with the previous frame
         */
        cor_coeff: f64,
        /*
         * log of intra_error
         */
        log_intra_error: f64,
        /*
         * log of coded_error
         */
        log_coded_error: f64,
    }

    impl AomFirstpass {
        #[cfg(test)]
        fn empty() -> AomFirstpass {
            AomFirstpass {
                frame: 0.0,
                weight: 0.0,
                intra_error: 0.0,
                frame_avg_wavelet_energy: 0.0,
                coded_error: 0.0,
                sr_coded_error: 0.0,
                lt_coded_error: 0.0,
                pcnt_inter: 0.0,
                pcnt_motion: 0.0,
                pcnt_second_ref: 0.0,
                pcnt_neutral: 0.0,
                intra_skip_pct: 0.0,
                inactive_zone_rows: 0.0,
                inactive_zone_cols: 0.0,
                MVr: 0.0,
                mvr_abs: 0.0,
                MVc: 0.0,
                mvc_abs: 0.0,
                MVrv: 0.0,
                MVcv: 0.0,
                mv_in_out_count: 0.0,
                new_mv_count: 0.0,
                duration: 0.0,
                count: 0.0,
                raw_error_stdev: 0.0,
                is_flash: 0,
                noise_var: 0.0,
                cor_coeff: 0.0,
                log_intra_error: 0.0,
                log_coded_error: 0.0,
            }
        }

        pub async fn read_aom_firstpass(
            reader: &mut (impl AsyncRead + Unpin),
            format: FirstpassFormat,
        ) -> Result<AomFirstpass, Error> {
            let mut record = [0u8; 30 * 8];
            reader
                .read_exact(&mut record[..format.record_size()])
                .await?;
            Ok(format.decode(&record))
        }

        pub async fn detect_format(file: &mut tokio::fs::File) -> Result<FirstpassFormat, Error> {
            let len = file.metadata().await?.len();
            let mut head = [0u8; 30 * 8];
            file.read_exact(&mut head).await?;
            Self::detect_format_from_head(&head, len).ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    "unsupported AOM first pass stats layout",
                )
            })
        }

        fn detect_format_from_head(head: &[u8], len: u64) -> Option<FirstpassFormat> {
            let field = |index: usize| {
                f64::from_ne_bytes(head[index * 8..index * 8 + 8].try_into().unwrap())
            };
            if field(0) != 0.0 {
                return None;
            }
            // `count` is 1.0 on the first record; it moves from field 22 to 23
            // when v3.14 inserts lt_coded_error.
            let old_count = field(22) == 1.0;
            let new_count = field(23) == 1.0;
            if new_count && !old_count {
                return Some(FirstpassFormat::V3_14);
            }
            if !old_count {
                return new_count.then_some(FirstpassFormat::V3_14);
            }
            if field(27) == 1.0 {
                Some(FirstpassFormat::V3_6)
            } else if field(29) == 1.0 {
                Some(FirstpassFormat::V3_7)
            } else if len.is_multiple_of(FirstpassFormat::V3_6.record_size() as u64)
                && !len.is_multiple_of(FirstpassFormat::V3_7.record_size() as u64)
            {
                Some(FirstpassFormat::V3_6)
            } else if len.is_multiple_of(FirstpassFormat::V3_7.record_size() as u64) {
                Some(FirstpassFormat::V3_7)
            } else {
                None
            }
        }

        fn second_ref_usage_thresh(frame_count_so_far: u64) -> f64 {
            let adapt_upto = 32;
            let min_second_ref_usage_thresh = 0.085;
            let second_ref_usage_thresh_max_delta = 0.035;
            if frame_count_so_far >= adapt_upto {
                min_second_ref_usage_thresh + second_ref_usage_thresh_max_delta
            } else {
                min_second_ref_usage_thresh
                    + (frame_count_so_far as f64 / (adapt_upto - 1) as f64)
                        * second_ref_usage_thresh_max_delta
            }
        }

        const VERY_LOW_INTER_THRESH: f64 = 0.05;
        const MIN_INTRA_LEVEL: f64 = 0.25;
        const INTRA_VS_INTER_THRESH: f64 = 2.0;
        const KF_II_ERR_THRESHOLD: f64 = 1.9;
        const ERR_CHANGE_THRESHOLD: f64 = 0.4;
        const II_IMPROVEMENT_THRESHOLD: f64 = 3.5;
        const BOOST_FACTOR: f64 = 12.5;
        const KF_II_MAX: f64 = 128.0;

        pub fn test_candidate_kf(
            self,
            last_stats: &AomFirstpass,
            future_frames: &VecDeque<AomFirstpass>,
            frame_since_last_scene: u64,
            num_mbs: u32,
            format: FirstpassFormat,
        ) -> bool {
            let next_stats = future_frames[0];

            let mut is_viable_kf = false;
            let pcnt_intra = 1.0 - self.pcnt_inter;
            let modified_pcnt_inter = self.pcnt_inter - self.pcnt_neutral;
            let second_ref_usage_thresh = Self::second_ref_usage_thresh(frame_since_last_scene);
            let frames_to_test_after_candidate_key = 16;
            let count_for_tolerable_prediction = 3;

            if frame_since_last_scene >= 3
                && (self.pcnt_second_ref < second_ref_usage_thresh)
                && (next_stats.pcnt_second_ref < second_ref_usage_thresh)
                && ((self.pcnt_inter < Self::VERY_LOW_INTER_THRESH)
                    || self.slide_transition(last_stats, next_stats)
                    || ((pcnt_intra > Self::MIN_INTRA_LEVEL)
                        && (pcnt_intra > (Self::INTRA_VS_INTER_THRESH * modified_pcnt_inter))
                        && ((self.intra_error / Self::double_divide_check(self.coded_error))
                            < Self::KF_II_ERR_THRESHOLD)
                        && (((last_stats.coded_error - self.coded_error).abs()
                            / Self::double_divide_check(self.coded_error)
                            > Self::ERR_CHANGE_THRESHOLD)
                            || ((last_stats.intra_error - self.intra_error).abs()
                                / Self::double_divide_check(self.intra_error)
                                > Self::ERR_CHANGE_THRESHOLD)
                            || ((next_stats.intra_error
                                / Self::double_divide_check(next_stats.coded_error))
                                > Self::II_IMPROVEMENT_THRESHOLD))))
            {
                let mut boost_score = 0.0;
                let mut old_boost_score = 0.0;
                let mut decay_accumulator = 1.0;
                let mut j = 0;
                for (i, local_next_frame) in future_frames
                    .iter()
                    .enumerate()
                    .take(frames_to_test_after_candidate_key)
                {
                    j = i + 1;

                    if format.has_lt_coded_error()
                        && (local_next_frame.intra_error - self.intra_error)
                            / Self::double_divide_check(self.intra_error)
                            > 0.1
                        && self.coded_error > local_next_frame.coded_error * 6.0
                    {
                        break;
                    }

                    let mut next_iiratio = Self::BOOST_FACTOR * local_next_frame.intra_error
                        / Self::double_divide_check(local_next_frame.coded_error);

                    if next_iiratio > Self::KF_II_MAX {
                        next_iiratio = Self::KF_II_MAX;
                    }

                    if local_next_frame.pcnt_inter > 0.85 {
                        decay_accumulator *= local_next_frame.pcnt_inter;
                    } else {
                        decay_accumulator *= (0.85 + local_next_frame.pcnt_inter) / 2.0;
                    }

                    boost_score += decay_accumulator * next_iiratio;

                    if (local_next_frame.pcnt_inter < 0.05)
                        || (next_iiratio < 1.5)
                        || (((local_next_frame.pcnt_inter - local_next_frame.pcnt_neutral) < 0.20)
                            && (next_iiratio < 3.0))
                        || ((boost_score - old_boost_score) < 3.0)
                        || (local_next_frame.intra_error < (200.0 / num_mbs as f64))
                    {
                        break;
                    }
                    old_boost_score = boost_score;
                }

                is_viable_kf = boost_score > 30.0 && (j > count_for_tolerable_prediction)
            }

            is_viable_kf
        }

        pub fn should_test_next_gop(self, future_frames: &VecDeque<AomFirstpass>) -> bool {
            std::iter::once(self)
                .chain(future_frames.iter().copied().take(31))
                .any(|stats| stats.frame > 2.0 && stats.lt_coded_error * 2.5 < stats.coded_error)
        }

        const VERY_LOW_II: f64 = 1.5;
        const ERROR_SPIKE: f64 = 5.0;
        fn slide_transition(self, last_frame: &AomFirstpass, next_frame: AomFirstpass) -> bool {
            (self.intra_error < (self.coded_error * Self::VERY_LOW_II))
                && (self.coded_error > (last_frame.coded_error * Self::ERROR_SPIKE))
                && (self.coded_error > (next_frame.coded_error * Self::ERROR_SPIKE))
        }

        fn double_divide_check(x: f64) -> f64 {
            if x < 0.0 {
                x - 0.000001
            } else {
                x + 0.000001
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::VecDeque;

        fn base_stats(frame: f64) -> AomFirstpass {
            let mut stats = AomFirstpass::empty();
            stats.frame = frame;
            stats
        }

        fn scene_candidate() -> AomFirstpass {
            let mut stats = base_stats(10.0);
            stats.intra_error = 1000.0;
            stats.coded_error = 1000.0;
            stats.pcnt_inter = 0.0;
            stats
        }

        fn predictable_future(frame: f64) -> AomFirstpass {
            let mut stats = base_stats(frame);
            stats.intra_error = 1000.0;
            stats.coded_error = 900.0;
            stats.pcnt_inter = 0.9;
            stats
        }

        #[test]
        fn detects_scene_cut_followed_by_predictable_frames() {
            let future_frames: VecDeque<_> = (11..15)
                .map(|frame| predictable_future(frame as f64))
                .collect();
            assert!(scene_candidate().test_candidate_kf(
                &base_stats(9.0),
                &future_frames,
                10,
                100,
                FirstpassFormat::V3_14,
            ));
        }

        #[test]
        fn stops_boost_scan_on_error_spike() {
            let mut spike = predictable_future(11.0);
            spike.intra_error = 2000.0;
            spike.coded_error = 100.0;
            let mut future_frames = VecDeque::from([spike]);
            future_frames.extend((12..15).map(|frame| predictable_future(frame as f64)));

            assert!(!scene_candidate().test_candidate_kf(
                &base_stats(9.0),
                &future_frames,
                10,
                100,
                FirstpassFormat::V3_14,
            ));
        }

        #[test]
        fn older_stats_keep_scanning_past_error_spike() {
            let mut spike = predictable_future(11.0);
            spike.intra_error = 2000.0;
            spike.coded_error = 100.0;
            let mut future_frames = VecDeque::from([spike]);
            future_frames.extend((12..15).map(|frame| predictable_future(frame as f64)));

            assert!(scene_candidate().test_candidate_kf(
                &base_stats(9.0),
                &future_frames,
                10,
                100,
                FirstpassFormat::V3_6,
            ));
        }

        #[test]
        fn detects_next_gop_signal() {
            let candidate = base_stats(10.0);
            let mut signal = base_stats(20.0);
            signal.lt_coded_error = 10.0;
            signal.coded_error = 30.0;
            assert!(candidate.should_test_next_gop(&VecDeque::from([signal])));

            let mut early_signal = base_stats(1.0);
            early_signal.lt_coded_error = 10.0;
            early_signal.coded_error = 30.0;
            assert!(!candidate.should_test_next_gop(&VecDeque::from([early_signal])));
        }

        fn encoded_record(format: FirstpassFormat) -> [u8; 30 * 8] {
            let mut record = [0u8; 30 * 8];
            let values: Vec<f64> = (0..30)
                .map(|index| index as f64 + 1.0)
                .filter(|value| {
                    !(format != FirstpassFormat::V3_14 && *value == 7.0)
                        && !(format == FirstpassFormat::V3_6 && *value >= 29.0)
                })
                .collect();
            assert_eq!(values.len() * 8, format.record_size());
            for (index, value) in values.iter().enumerate() {
                record[index * 8..index * 8 + 8].copy_from_slice(&value.to_ne_bytes());
            }
            record
        }

        #[test]
        fn decodes_every_stats_layout() {
            for format in [
                FirstpassFormat::V3_6,
                FirstpassFormat::V3_7,
                FirstpassFormat::V3_14,
            ] {
                let decoded = format.decode(&encoded_record(format));
                assert_eq!(decoded.frame, 1.0);
                assert_eq!(decoded.sr_coded_error, 6.0);
                assert_eq!(decoded.pcnt_inter, 8.0);
                assert_eq!(decoded.inactive_zone_cols, 14.0);
                assert_eq!(decoded.cor_coeff, 28.0);
                if format.has_lt_coded_error() {
                    assert_eq!(decoded.lt_coded_error, 7.0);
                    assert_eq!(decoded.log_coded_error, 30.0);
                } else {
                    assert_eq!(decoded.lt_coded_error, 0.0);
                }
                if format == FirstpassFormat::V3_6 {
                    assert_eq!(decoded.log_intra_error, 0.0);
                    assert_eq!(decoded.log_coded_error, 0.0);
                } else {
                    assert_eq!(decoded.log_intra_error, 29.0);
                    assert_eq!(decoded.log_coded_error, 30.0);
                }
            }
        }

        fn head_with(fields: &[(usize, f64)]) -> [u8; 30 * 8] {
            let mut head = [0u8; 30 * 8];
            for (index, value) in fields {
                head[index * 8..index * 8 + 8].copy_from_slice(&value.to_ne_bytes());
            }
            head
        }

        #[test]
        fn detects_stats_layout_from_first_record() {
            let v3_6 = head_with(&[(22, 1.0), (27, 1.0)]);
            assert_eq!(
                AomFirstpass::detect_format_from_head(&v3_6, 0),
                Some(FirstpassFormat::V3_6)
            );

            let v3_7 = head_with(&[(22, 1.0), (29, 1.0)]);
            assert_eq!(
                AomFirstpass::detect_format_from_head(&v3_7, 0),
                Some(FirstpassFormat::V3_7)
            );

            let v3_14 = head_with(&[(22, 416_666.0), (23, 1.0)]);
            assert_eq!(
                AomFirstpass::detect_format_from_head(&v3_14, 0),
                Some(FirstpassFormat::V3_14)
            );
        }

        #[test]
        fn falls_back_to_log_length_for_single_frame_clips() {
            let head = head_with(&[(22, 1.0)]);
            assert_eq!(
                AomFirstpass::detect_format_from_head(&head, 27 * 8 * 2),
                Some(FirstpassFormat::V3_6)
            );
            assert_eq!(
                AomFirstpass::detect_format_from_head(&head, 29 * 8 * 2),
                Some(FirstpassFormat::V3_7)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::aom_firstpass::aom::{AomFirstpass, FirstpassFormat};
    use std::io::Cursor;

    #[test]
    fn test_firstpass_size_matches_aom() {
        assert_eq!(std::mem::size_of::<AomFirstpass>(), 30 * 8);
    }

    #[tokio::test]
    pub async fn test_firstpass_readout() {
        let record_size = std::mem::size_of::<AomFirstpass>();
        let mut data = vec![0u8; record_size * 2];
        data[..8].copy_from_slice(&0.0f64.to_ne_bytes());
        data[record_size..record_size + 8].copy_from_slice(&1.0f64.to_ne_bytes());
        let mut test_file = Cursor::new(data);
        let frame1 = AomFirstpass::read_aom_firstpass(&mut test_file, FirstpassFormat::V3_14)
            .await
            .unwrap();
        let frame2 = AomFirstpass::read_aom_firstpass(&mut test_file, FirstpassFormat::V3_14)
            .await
            .unwrap();

        assert_eq!(0.0, frame1.frame);
        assert_eq!(1.0, frame2.frame);
    }
}
