//! English. To add a language, copy this file, translate the right-hand
//! sides, and register it in `text/mod.rs`. `{0}` marks where a value goes.

use super::Text;

pub const TEXT: Text = Text {
    // Settings menu
    settings: "Settings",
    tab_picture: "Picture",
    tab_stream: "Stream",
    tab_device: "Device",
    tab_console: "Console",
    sharpness: "Sharpness",
    saturation: "Saturation",
    contrast: "Contrast",
    resolution: "Resolution",
    bitrate: "Bitrate limit",
    display_sync: "Display sync",
    sync_vsync: "V-Sync",
    sync_low_latency: "Low latency",
    smoothing: "Smoothing",
    volume: "Volume",
    rumble: "Vibration",
    statistics: "Statistics",
    on: "On",
    off: "Off",
    muted: "Muted",
    switch_console: "Switch console",
    switch_console_about: "Play on a different Xbox",
    restart_console: "Restart console",
    restart_console_about: "Helps when the console stops responding",
    sign_out: "Sign out",
    sign_out_about: "Forget this Microsoft account",
    confirm: "Press again to confirm",
    defaults: "Defaults",
    quit: "Quit",

    // One line under each setting. Keep these short: about 60 letters fit.
    about_sharpness: "Makes edges crisper. Too much looks grainy.",
    about_saturation: "How vivid the colours are.",
    about_contrast: "The difference between dark and bright areas.",
    about_resolution: "1080p is sharper, 720p is lighter. Applies on closing.",
    about_bitrate: "Higher is cleaner, lower is steadier. Applies on closing.",
    about_display_sync: "V-Sync avoids tearing. Low latency shows frames sooner.",
    about_smoothing: "Evens out network hiccups at the cost of a little delay.",
    about_volume: "Loudness of the game sound on this device.",
    about_rumble: "Lets games shake the controller.",
    about_statistics: "Shows frame rate, bitrate and delay in the corner.",
    about_switch_console: "Currently connected to {0}.",
    about_restart_console: "Closes whatever is running on the console.",
    about_sign_out: "You will need to sign in again.",

    // Sign-in
    sign_in_title: "Sign in to Xbox",
    sign_in_body: "Open {0} on your phone or computer and enter this code, or scan it.",
    sign_in_waiting: "Waiting for you to approve the sign-in",
    open_browser: "Open browser",
    copy_code: "Copy code",
    copied: "Copied",

    // Choosing a console
    pick_console_title: "Choose your console",
    pick_console_body: "There is more than one Xbox on this account.",
    no_console_title: "No console found",
    no_console_body: "This account has no Xbox set up for remote play. On the console, open \
        Settings > Devices & connections > Remote features and turn on \"Enable remote features\". \
        The console must be signed in to the same Microsoft account.",

    // Failures
    failure_title: "Could not connect",
    try_again: "Try again",
    copy_error: "Copy error",
    remote_features_off: "Remote play is turned off on the console. On the Xbox, open \
        Settings > Devices & connections > Remote features and turn on \"Enable remote features\".",
    console_off: "The console is switched off. Turn it on, and choose the Sleep power option \
        (Settings > General > Power options) so it can be woken up remotely next time.",
    console_unreachable: "The Xbox service cannot reach the console. Check that it is on or \
        asleep (not fully shut down), connected to the network, and has remote features enabled.",
    console_deaf: "The console is not accepting remote commands right now. Restart it by hand: \
        hold the Xbox button on the controller and choose \"Restart console\".",
    restart_failed: "The console could not be restarted",

    // Error report
    report_error: "error",
    report_log: "Log",
};
