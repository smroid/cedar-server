# July 2024

Cedar-server version: 0.1.0

Initial public release.

# August 28 2024

Cedar-server version: 0.2.0

Major updates:

* Expand saved preferences to cover more items, such as observer location.

* Fix "white flash" that occurs when slewing.

* Improve SkySafari integration:
  * Enable "Sync" feature
  * Accept observer geolocation information

# September 7 2024

Cedar-server version: 0.3.0

Major updates:

* New "daylight" alignment mode. During the day, point the telescope at a
  distant landmark, tap Cedar's new "zoom" button, and then tap on the screen
  to tell Cedar what the telescope is aimed at.

* Basic vs. advanced functionality.

* Removed exposure time control. This is now entirely automatic.

* Text size setting.

* Improved night vision theme. Deeper reds, darker panel backgrounds.

* Replaced slider for showing number of detected stars with a circular gauge.
  Tapping the gauge brings up performance stats.

* In setup screen, added enable/disable for focus aids.

# September 21 2024

Cedar-server version: 0.4.0

Major updates:

* 'About' screen giving information about Cedar server and calibration
  results.

* Minor UI improvements such as better tap target sizes.

* UI alert for lack of server connectivity or lack of detected camera.

* Improved menu item styling.

* Listen on port 80 (and also 8080 as before).

* "Demo mode" allowing selectable image files to be used instead of camera.

* Use a "run" directory instead of "src" directory.

* Use Raspberry Pi activity LED to convey Cedar status.
  * blinking: waiting for client to connect
  * off: client is connected

  When Cedar is not running, the activity LED reverts to reflecting
  SD card activity

* Add restart option in addition to shutdown.

# October 12 2024

Cedar-server version: 0.5.0

* Add preference for inverting camera image (rotate 180degrees).

* Fix exposure calibration logic bug.

* Telescope boresight no longer restricted to central third of image.

* Redesign setup mode: focus, then align. Focus and align have
  on-screen prompts; align highlights 3 brightest detections.

* Daylight mode is now applicable to focus as well as align.

* Remove speed/accuracy slider.

* Fix push-to directions layout problems.

# November 15 2024

Cedar-server version: 0.6.0

* Improve server and network reliability.

* Adjust camera gain in daytime focus and align modes.

* Fix bug leading to very long calibration delays.

* Change to rolling logs.

# November 17 2024

Cedar-server version: 0.6.1

* Bug fixes.

# December 15 2024

Cedar-server version: 0.7.0

* Preference for left vs right handed control placement.

* Compact layout for sky location display.
  * Can tap to bring up comprehensive information.
  * Can designate preferred display: ra/dec vs alt/az

* Add preference for screen always on (app only).

* In Setup alignment mode, rotate displayed image to orient zenith up.

* Pinch zoom to change text size.

* Eliminate network-releated hangs (hopefully) trying to fetch fonts.

* Add star count and image noise to performance popup.

* Fix missing alt/az vs. equatorial mount in preferences.

# January 7 2025

Cedar-server version: 0.8.0

* Fix bugs in calibration cancellation logic.

* Remove camera temperature attribute.

* Fix bug in Rpi camera logic regarding discarding images after setting change.

* In align screen, ensure that bright star is highlighted as alignment target
  even if it is so overexposed that it is not detected as a star.

* Fix bug where activity light would blink when Cedar Aim is inactive because
  user is interacting instead with SkySafari.

* Reduce geolocation map resolution.

# January 13 2025

Cedar-server version: 0.8.1

* Fix bug that was hiding geolocation button.

* Update camera logic for Rpi5 compressed raw.

# February 15 2025

Cedar-server version: 0.9.0

* Fixes for high resolution camera modules such as the RPi HQ camera:
  * When downsampling image for display on phone, preserve star brightness.
  * Fix hit testing in alignment screen to make it easier to select
    alignment star.
  * Adjust size of focus assist inset image.

* Improve threading and locking.

* Fix bugs causing 100% CPU usage even when updating at low frequency.

* Fix TelescopePosition implementation to avoid "split updates" in
  SkySafari, eliminating occasional spurious position jumps.

* When calibrating plate solver, use longer exposure to get more stars
  for improved FOV/distortion estimates.

* Goto mode:
  * Show "Re-align" button only when close to target.
  * Adjust slew directions text block placement.
  * Add small icons for up/down and clockwise/counterclockwise.

* Reorganize Rust package structure under cedar-server component.

* Refactor Tetra3 dependency.
  * Move subprocess management into tetra3_server directory.
  * Implement new SolverTrait to use Tetra3 (Cedar-solve).
  * Update protobuf types so that Cedar does not directly depend
    on Tetra3 protobuf types.

# February 20 2025

Cedar-server version: 0.9.1

* Fixes bug where plate solve failures could cause SkySafari to
  stop getting updates until Cedar Aim app is resumed.

* Increases maximum exposure time for color cameras to 2 seconds.

# May 26 2025

Cedar-server version: 0.9.3

* Update plate solver match_threshold to 1e-5, reducing the chance of
  a false solution.

* Fix hanging behavior in solver.

* Improve auto-exposure algorithms.

* Always rotate image to zenith up when not in focus or daytime align mode.

* If calibration fails, stay in current mode (focus or daytime align).

* Other calibration fixes.

* Add interstitial explanation screens for startup, focus, and align.

* Broaden target tap tolerance in align mode.

* Increase text sizes, tweak layouts.

* Improve formatting of push-to directions.

# June 23 2025

Cedar-server version: 0.10.0

* Add more information screens.

* Add "don't show again" checkbox on information screens.

* When changing camera parameters, continue to process images that
  don't yet reflect the parameter change.

* Tweak hot pixel detection logic.

* Auto exposure improvements.

* Update calibration and auto exposure logic to not raise
  exposure too high when trying to increase star count. This
  might fix reported "white out" problems.

* Fix occasional hang on calibration failure.

# July 1 2025

Cedar-server version: 0.11.0

* Display central square crop of camera image instead of full camera image.

* Adjust app layout to take advantage of larger data/control panels around
  square image.

* In about screen camera information, reflect when demo image is being used.

* Don't hold camera lock when waiting for next exposure to be ready; this was
  causing UI responsiveness problems.

* Clean up other locking issues.

* Once LED turns off (when app connects), don't turn LED back on (e.g. when app
  goes to sleep due to screen off etc.).

* Make FOV bullseye more visible in daylight align mode.

* Add haptic feedback in align mode and for re-align.

# August 2 2025

Cedar-server version: 0.12.2

* Update/fix auto-exposure logic.

* Update/fix exposure calibration logic.

* Star count gauge displays moving average rather than instantaneous value.

* Fix daylight mode auto-exposure.

* On exposure calibration failure, UI pops a message identifying reason:
  Too few stars due to lens cap being covered; sky too bright.

* Improved twilight tolerance.

# November 25 2025

Cedar-server version: 0.16.0

* Reorganize "don't show" preference items.

* Allow perf gauge display item to be chosen.

* Daylight focus zoom image.

* Improve server mutex and async logic.

* Improve About screen organization.

* Remove update rate slider (always runs at full speed).

* Draw slew directions in data area instead of overlayed on main image.

* Fix determination of whether slew target is in view.

* Long press on shutdown confirmation clears observer location.

* Fix full screen layout on Pixel phone.

* Layout tweaks for iOS.

# December 15 2025

* Improve focus screen prompts.

* Improve alignment screen prompts.

* Popup feedback when user taps on align target.

Plus contributions from a member of the Cedar user community:

* Support LX200 protocol for SkySafari 6 and 7 and Stellarium.

* Support Bluetooth for SkySafari to connect to Cedar server.

December 22 2025

* LX200 fixes and improvements.

* Bluetooth fixes and improvements.
