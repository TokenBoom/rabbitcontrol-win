use ironrdp_displaycontrol::pdu::MonitorLayoutEntry;

// Use the protocol's validated primary-monitor dimensions for the local framebuffer.
pub fn initial_monitor() -> Result<MonitorLayoutEntry, Box<dyn std::error::Error>> {
    Ok(MonitorLayoutEntry::new_primary(1280, 720)?)
}
