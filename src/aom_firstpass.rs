pub mod aom {
    use tokio::io::{AsyncRead, AsyncReadExt, Error};
    use core::mem;

    #[repr(C)]
    pub struct AomFirstpass {
        frame: f64,
        weight: f64,
        intra_error: f64,
        frame_avg_wavelet_energy: f64,
        coded_error: f64,
        sr_coded_error: f64,
        tr_coded_error: f64,
        pcnt_inter: f64,
        pcnt_motion: f64,
        pcnt_second_ref: f64,
        pcnt_third_ref: f64,
        pcnt_neutral: f64,
        intra_skip_pct: f64,
        inactive_zone_rows: f64,
        inactive_zone_cols: f64,
        MVr: f64,
        mvr_abs: f64,
        MVc: f64,
        mvc_abs: f64,
        MVrv: f64,
        MVcv: f64,
        mv_in_out_count: f64,
        new_mv_count: f64,
        duration: f64,
        count: f64,
        raw_error_stdev: f64,
        is_flash: i64,
        noise_var: f64,
        cor_coeff: f64
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
                tr_coded_error: 0.0,
                pcnt_inter: 0.0,
                pcnt_motion: 0.0,
                pcnt_second_ref: 0.0,
                pcnt_third_ref: 0.0,
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
                cor_coeff: 0.0
            }
        }

        async fn readAomFirstpass(mut reader: impl AsyncRead + Unpin) -> Result<AomFirstpass, Error> {
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