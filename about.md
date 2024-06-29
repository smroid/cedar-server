# About Cedar-server

Cedar-server implements a plate solving pipeline to help you aim your telescope.

Cedar-server's design provides rich functionality, high performance, reliable
operation, and simple usage. To the greatest extent possible, all internal
settings and calibrations are performed automatically so the user does not need
to tweak endless knobs in a hunt for good results.

## Processing pipeline

Cedar carries out its activities in a pipelined fashion:

* Stage 0: image sensor is integrating exposure N.

* Stage 1: camera module is converting exposure N-1 from RAW to 8-bit monochrome.

* Stage 2: Cedar-detect algorithm is procesing exposure N-2:
  * removing hot pixels
  * binning to a favorable resolution for detecting stars
  * detecting stars
  * finding centroids for detected stars

* Stage 3: Cedar-solve algorithm is plate solving star centroids extracted
  from exposure N-3.

* Stage 4: Upon client request, Cedar-server logic serves information from
  exposure N-4.

Cedar executes all stages of the pipeline concurrently. This is a good fit
to the Raspberry Pi, which uses a quad core processor so the pipeline stages run
in parallel.


## Operating modes

Cedar has two primary modes of operation: Setup and Aim.

### Setup

Setup mode is what the user first sees in the Cedar-aim app. Setup mode
provides:

* Visual support for focusing the camera. This is presented in the Cedar-aim
  user interface as a magnified view of the brightest star in the central
  region of the field of view (FOV), displayed at a high refresh rate.

* "Any star" boresight alignment. The user centers the central brightest star in
  the telescope view and then taps a button to capture the x/y position of the
  telescope boresight.

* Daylight boresight alignment (coming soon). In daylight, the user aims the
  telescope at a landmark such as a distant light pole or corner of a building,
  then taps on the corresponding item on a magnified view of the central region
  of the FOV.

### Aim

Aim mode (referred to as Operate mode in the Cedar-server code) is the main
operating mode, where:

* Plate solves are done continuously, updating the RA/Dec information on the
  Cedar-aim UI.

* Move to target: see below.

* Polar alignment (coming soon). Cedar provides assistance for the "declination
  drift" method of polar alignment, using the high precision of plate solutions
  and integration over time to provide polar axis pointing corrections.

## Calibrations

When the user transitions from Setup mode to Aim mode, Cedar takes the
opportunity to perform a series of calibrations, on the presumption that the
camera is pointed at a star field and is well focused. The calibrations are as
follows, taking several seconds in total:

* Camera offset (black level): To avoid black crush.

* Exposure duration: Cedar-detect is used to see how many stars are detected
  with a starting exposure duration. The exposure duration is adjusted upward or
  downward to obtain a desired number of stars (typically 20).

* Lens characteristics: Cedar does a trial plate solve, passing null constraints
  for field of view and lens distortion, with a generous solve timeout. Given
  the calibrated exposure duration, there should be a proper number of
  centroids, leading to a good initial solution, even if slow because of the
  lack of FOV constraint. The actual FOV and lens distortion parameters are
  obtained and used for subsequent Aim mode plate solves, yielding faster
  solves.

* Solver parameters: The trial plate solve in the previous calibration step also
  yields a sense of how long solves take on this system, allowing a suitable
  solve timeout to be determined for Aim mode solves. In addition, the residual
  centroid position error (even after using the calibrated lens distortion) is
  captured to allow determination of the plate solver's 'match_max_error'
  parameter (coming soon).

## Moving to target

Cedar-server can provide push-to guidance for a given sky target. The user
initiates this by using SkySafari or entering RA/Dec manually in Cedar-aim UI
(coming soon).

Cedar decomposes the needed telescope motion in terms of the telescope's mount
axes, either north-south and east-west (equatorial mount) or up-down and
clockwise-counterclockwise (alt-az mount).

After centering the telescope on the target, before exiting move-to mode, the
user can direct Cedar-server to refine its boresight alignment.

## Camera handling

Cedar-server currently supports ASI cameras over USB (tested with ASI120mm
mini), and Raspberry Pi cameras via the CSI ribbon cable (tested with Rpi High
Quality camera).

### Resolutions

Cedar-server has been tested with cameras ranging in resolution from 1.2
megapixel to 12.3 megapixel. Cedar-server logic automatically uses varying
amounts of software binning to obtain a reduced resolution suitable for
Cedar-detect's star detection algorithms; note that centroiding is always done
on the full resolution original image for best accuracy.

### Gain

Cedar-server operates each kind of camera at its optimal gain setting. The
optimal gain is taken to be the lowest gain that yields RMS noise of around 0.5
ADU on a dark image taken at a typical plate solving exposure time. The optimal
gain values are determined by offline trials and are baked into the Cedar-camera
library.

### Color camera

When a color camera (such as the Rpi HQ camera) is used, the Cedar-camera
library does not debayer to color or monochrome, but instead returns the full
resolution RAW bayer mosaic image as 8 bits intensity value (linear) per photosite.

Retaining the image in RAW form allows Cedar-detect to accurately detect and
remove hot pixels (see below). The software binning used prior to star detection
acts as a simplistic but effective conversion to monochrome.

### Readout format

All logic in Cedar-detect and Cedar-server use 8-bit linear pixel intensity
encoding. The Cedar-camera library either configures the camera for 8-bit RAW
readout, or if only 10- or 12-bit RAW readout is supported, the Cedar-camera
library converts to 8-bit RAW.

### Hot pixel detection

As mentioned earlier, the camera readout is full-resolution RAW with no
debayering (if color). Any noise reduction modes provided by the camera driver
are disabled.

Cedar-detect examines each pixel and its left/right neighbors to detect
hot pixels. A pixel is hot if:

* It is brighter than the local background by an amount that is significant
  w.r.t. the noise, and

* The pixel's left+right pixels have low background-corrected intensity
  compared to the center pixel.

The rationale is that hot pixels are isolated, whereas genuine star images are
spread out over many pixels, so the neighbors of a star's central pixel will
also be relatively bright. See Cedar-detect (`gate_star_1d()` function in
algorithm.rs) for details.

When a hot pixel is detected, it is replaced by the mean of its left/right
neighbors. For efficiency, Cedar-detect combines hot pixel processing with the
binning used prior to star detection.

## Auto exposure

The optimum exposure time depends on many factors:

* Camera model

* Lens attached (focal length, f/ratio)

* Sky conditions

* Cedar operating mode

Instead of requiring the user to set the camera exposure time, Cedar-server
automatically adjusts the exposure time in a mode-specific fashion.

### Daylight alignment mode

In daylight alignment mode (coming soon), the camera is pointed at a
daylight-illuminated terrestrial scene. In this situation Cedar-server adjusts
the exposure time to achieve good brightness of the central region of the field
of view.

### Setup mode (focusing)

To achieve a high frame rate during focusing, Cedar-server underexposes the
brightest star of the central region of the field of view. A crop of the
brightest star is then stretched for display.

### Aim mode (plate solving)

When plate solving, Cedar-server adjust the exposure time to achieve a desired
number of detected stars (see below).

## Speed vs accuracy slider

Reliable plate solving requires a good number of correctly detected stars with
reasonably accurate brightness estimates. Cedar-solve can succeed with as few as
6 detected stars, but is much more reliable at above 10 stars. In practice using
20 detected stars yields solid solve results; using more than 20 stars provides
little benefit but requires longer exposure times.

The number of detected stars is influenced by:

* Exposure time. A longer exposure produces higher signal-to-noise and thus allows
  more numerous fainter stars to be detected.

* Noise-relative detection threshold. A "sigma multiple" parameter governs the
  sensitivity of Cedar-detect. A high sigma value yields fewer star detections
  but very few false positives; a lower sigma value allows fainter stars to be
  detected but also results in some noise fluctuations to be mistaken for stars.

So we have potentially three knobs to present to the user:

1. Desired number of detected stars for plate solving.

2. Exposure time.

3. Detection "sigma multiple" parameter.

These are interrelated, as items 2 and 3 together influence the number of
star detections, which relates to item 1.

Instead of having these knobs, Cedar-server instead provides a simple speed vs.
accuracy knob with three settings:

* Balanced: Baseline values (see below) for desired number of stars and detection sigma
  are used.

* Faster: Baseline values for star count and detection sigma are multiplied by 0.7.

* Accurate: Baseline values for star count and detection sigma are multiplied by 1.4.

In each case, auto-exposure logic determines the exposure time to acheive the
desired number of stars.

In the "faster" case, we are seeking fewer stars so Cedar-server will use
shorter exposures. Furthermore, the lowered sigma value allows the exposure time
to be lowered yet more because it is "easier" to detect stars (plus false
positives).

In the "accurate" case, we are seeking more stars at a higher sigma threshold,
so Cedar-server will use longer exposures. By detecting more stars with fewer
false positives, plate solutions are more robust and the resulting astrometric
accuracy will be increased due to the larger number of matches.

The baseline value for desired number of stars is 20; the baseline value for
detection sigma multiple is 8. These can be overridden on the Cedar-server
command line.

## Motion analysis

Cedar-server includes logic that tracks the plate solutions over time, allowing
additional functionality to be synthesized. A basic concept is "dwell
detection", where successive plate solutions yielding (nearly) unchanging
results allow Cedar-server to conclude that the telescope is not moving.

### Mount type determination

During a dwell, if the declination value is unchanging and the right ascension
is changing at the sidereal rate, Cedar-server can infer that the telescope
mount is non-motorized.

If the RA and Dec are both unchanging, Cedar-server can infer that the telescope
mount is equatorial with clock drive.

### Adaptive frame rate

With a sensitive camera such as the ASI120mm mini and a fast lens, Cedar-server
can run at frame rates in the range of 10-30Hz. This causes high CPU utilization
because the processing pipeline keeps the multiple Rpi cores busy.

(coming soon) To improve battery life, Cedar-server reduces the frame rate when
a dwell persists for more than a few seconds. Once motion is again detected,
Cedar-server returns to its high frame rate.

### Polar alignment

When dwelling with a clock-driven equatorial mount that is accurately polar
aligned, the RA/Dec values will be perfectly stationary. However, if the polar
alignment is off, a "drift" in the declination value will be observed; generally
the RA drift is small unless the polar axis is grossly misalgined.

Cedar-server measures the declination drift rate during dwells and uses this
information, along with knowledge of where the telescope is pointed relative to
the celestial equator and meridian, to quantitatively determine how the polar
alignment should be corrected.

See [Canburytech.net](https://canburytech.net/DriftAlign/index.html) for a detailed
explanation of how declination drift is used to correct polar alignment.

## SkySafari integration

Cedar-server implements the [Ascom Alpaca](https://ascom-standards.org/About/Index.htm)
protocol to present itself as a "telescope" that reports its RA/Dec and responds
to slew requests. This allows the user to connect SkySafari to Cedar-server,
after which SkySafari shows the telescope's position and allows the user to
initiate moving to a target.
