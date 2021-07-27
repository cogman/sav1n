pub mod aom {
    use tokio::io::{AsyncRead, AsyncReadExt, Error};
    use core::mem;

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
    }

    impl AomFirstpass {
        fn empty() -> AomFirstpass {
            return AomFirstpass {
                frame: 0.0,
                weight: 0.0,
                intra_error: 0.0,
                frame_avg_wavelet_energy: 0.0,
                coded_error: 0.0,
                sr_coded_error: 0.0,
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
            };
        }

        pub async fn readAomFirstpass(reader: &mut (impl AsyncRead + Unpin)) -> Result<AomFirstpass, Error> {
            let mut firstpass = AomFirstpass::empty();
            unsafe {
                let buffer: &mut [u8] = std::slice::from_raw_parts_mut(
                    &mut firstpass as *mut AomFirstpass as *mut u8,
                    mem::size_of::<AomFirstpass>(),
                );
                reader.read_exact(buffer).await?;
            }
            return Ok(firstpass);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use crate::aom_firstpass::aom::AomFirstpass;

    // Contains 2 AOM frame stats
    static RAW_FRAME_DATA: [u8; 432] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xA0, 0x88, 0x13, 0xE7, 0x0A, 0xAB, 0x03, 0x40, 0xA7, 0xA1, 0xC8, 0xA6, 0xF0, 0x40, 0x25, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0xBF, 0xA7, 0xA1, 0xC8, 0xA6, 0xF0, 0x40, 0x25, 0x40, 0xA7, 0xA1, 0xC8, 0xA6, 0xF0, 0x40, 0x25, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2E, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x68, 0x6E, 0x19, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0xA0, 0x88, 0x13, 0xE7, 0x0A, 0xAB, 0x03, 0x40, 0xA7, 0xA1, 0xC8, 0xA6, 0xF0, 0x40, 0x25, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0xBF, 0xE4, 0x3F, 0xF4, 0x16, 0x11, 0x18, 0x17, 0x40, 0xE4, 0x3F, 0xF4, 0x16, 0x11, 0x18, 0x17, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF9, 0xC5, 0x92, 0x5F, 0x2C, 0xF9, 0xEF, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2E, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6C, 0x6E, 0x19, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F];

    #[tokio::test]
    pub async fn test_firstpass_readout() {
        let mut data = Vec::new();
        data.extend_from_slice(&RAW_FRAME_DATA);
        let mut test_file = Cursor::new(data);
        let frame1 = AomFirstpass::readAomFirstpass(&mut test_file).await.unwrap();
        let frame2 = AomFirstpass::readAomFirstpass(&mut test_file).await.unwrap();

        assert_eq!(0.0, frame1.frame);
        assert_eq!(1.0, frame2.frame);
    }
}