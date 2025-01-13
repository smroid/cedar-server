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

November 17 2024

Cedar-server version: 0.6.1

* Bug fixes.

December 15 2024

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

January 7 2025

Cedar-server version: 0.8.0

* Fix bugs in calibration cancellation logic.

* Remove camera temperature attribute.

* Fix bug in Rpi camera logic regarding discarding images after setting change.

* In align screen, ensure that bright star is highlighted as alignment target
  even if it is so overexposed that it is not detected as a star.

* Fix bug where activity light would blink when Cedar Aim is inactive because
  user is interacting instead with SkySafari.

* Reduce geolocation map resolution.

January 13 2025

Cedar-server version: 0.8.1

* Fix bug that was hiding geolocation button.

* Update camera logic for Rpi5 compressed raw.
