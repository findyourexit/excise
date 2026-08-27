use ::std::fmt;

pub struct DisplaySize(pub f64);

impl fmt::Display for DisplaySize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 999_999_999.0 {
            write!(f, "{:.1}G", self.0 / 1_073_741_824.0) // 1024 * 1024 * 1024
        } else if self.0 > 999_999.0 {
            write!(f, "{:.1}M", self.0 / 1_048_576.0) //  1024 * 1024
        } else if self.0 > 999.0 {
            write!(f, "{:.1}K", self.0 / 1024.0)
        } else {
            // Carry the unit at every magnitude: a bare "37" beside "1.2K" reads
            // as a count of entries rather than a size.
            write!(f, "{}B", self.0)
        }
    }
}
