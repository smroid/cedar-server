# Polar alignment advice

Cedar can help you polar align your clock-driven equatorial mount. It does
this by telling you how far you need to raise or lower your telescope's polar
axis, and how much you need to move the polar axis to the left or right.

Currently this feature is a bit of an Easter egg; a future version of Cedar will
make the polar alignment feature more discoverable.

## Prerequisites

In order to enable Cedar's polar alignment support, the following conditions
must all be met:

* Cedar must know your geographic location. Open Cedar Aim's menu, and look for
  the item that is either "Location unknown" or "Location `<lat>` `<long>`". If the
  location is unknown, tap on it to bring up a world map to enter your
  approximate location.

* Your telescope mount must be equatorial type and must be clock-driven at
  sidereal rate.

* Your telescope mount must be roughly polar aligned, say to within 15 or 20
  degrees of the celestial pole in both altitude and azimuth.

* Cedar must be seeing the sky and producing plate solutions.

When these prerequisites are met, you can request polar alignment advice. You do
this separately for azimuth (adjusting polar axis left or right) and altitude
(adjusting polar axis up or down), as described next.

## Triggering polar alignment advice

### Azimuth

To find out whether you need to move your mount's polar axis to the left or to
the right, you trigger Cedar's polar alignment azimuth advice. You do this by
pointing your telescope to the celestial equator (zero declination, plus or
minus 15 degrees), and to the meridian (zero hour angle, plus or minus one
hour). Note that the hour angle is given under the RA/Dec values on Cedar's aim
mode screen.

Once you've moved your telescope to the zone around the meridian at the
celestial equator, leave the telescope motionless (aside from the clock drive).
Within a few seconds Polar Align "az" information will appear, with an estimate
of the number of degrees left or right that you need to move your mount's polar
axis. The longer you let the telescope sit the more accurate the estimate will
become.

Move your polar axis left or right by the indicated amount, and wait again to
see if the azimuth error has diminished. You can easily achieve a small fraction
of a degree error within a minute or so.

### Altitude

To find out whether you need to move your mount's polar axis up or down, you
trigger Cedar's polar alignment altitude advice. You do this by pointing your
telescope along the celestial equator, either near the east horizon or near the
west horizon (within two hours in each case).

Now leave the telescope motionless, and soon you'll see Polar Align information,
this time with the "alt" error estimate. Wait a bit for the estimate to firm up,
then move your mount's polar axis up or down by the indicated amount. Repeat a
couple of times until the altitude error has been reduced to near zero.

At this point you might want to repeat the Azimuth polar alignment (pointing at
celestial equator at meridian); it might have changed a bit due to the polar
altitude adjustment.
