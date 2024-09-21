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
