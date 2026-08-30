pub fn init(json: bool) { let builder=tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()); if json { builder.json().init(); } else { builder.init(); } }
