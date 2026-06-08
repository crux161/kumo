use imager::{ImageArch, ImagePlan};

fn main() {
    let plan = ImagePlan::for_arch(".", ImageArch::Aarch64);
    print!("{}", plan.manifest());
}
