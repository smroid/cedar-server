# About Cedar-server

As noted in the [README.md](README.md) file, Cedar-server implements a plate solving
pipeline to help you aim your telescope.

Cedar-server's design provides rich functionality, high performance, and simple
usage. To the greatest extent possible, all internal settings and calibrations
are performed automatically so the user does not need to tweak endless knobs in
a hunt for good results.

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

* Slew-to-target: initiated from SkySafari, or manual entry of Ra/Dec (coming
  soon), or from Cedar-sky (coming soon). Cedar displays distance and direction
  to target and displays the needed telescope axis motions.

* Polar alignment (coming soon). Cedar provides assistance for the "declination
  drift" method of polar alignment, using the high precision of plate solutions
  and integration over time to quickly provide polar axis pointing corrections.

## Calibrations

When the user transitions from Setup mode to Aim mode, Cedar takes the
opportunity to perform a series of calibrations, on the assumption that the
camera is pointed at a star field and is well focused. The calibrations are as
follows, taking several seconds in total:

* Camera offset (black level): To avoid black crush.

* Exposure duration: Cedar-detect is used to see how many stars are detected
  with a starting exposure duration. The exposure duration is adjusted upward or
  downward to obtain a desired number of stars (typically 20).

* Lens characteristics: Cedar does a trial plate solve, passing null constraints
  for field of view and lens distortion, with a generous solve timeout. Given
  the calibrated exposure duration, there should be a proper number of
  centroids, leading to a good initial solution, if slow because of the lack of
  FOV constraint. The actual FOV and lens distortion parameters are extracted
  and used for Aim mode plate solves, yielding the fastest possible solves.

* Solver parameters: The trial plate solve in the previous calibration step also
  yields a sense of how long solves take on this system, allowing a suitable
  solve timeout to be determined for Aim mode solves. In addition, the residual
  centroid position error (even after using the calibrated lens distortion) is
  captured to allow determination of the plate solver's 'match_max_error'
  parameter (coming soon).

## Moving to target

Cedar-server can provide push-to guidance for a given sky target. The user
initiates this by using SkySafari, entering RA/Dec manually (coming soon), or
using the integrated Cedar-sky app (coming soon).

Cedar decomposes the needed telescope motion in terms of the telescope's mount
axes, either north-south and east-west (equatorial mount) or up-down and
clockwise-counterclockwise (alt-az mount).

After centering the telescope on the target, before existing move-to mode, the
user can refine the boresight alignment.

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


### Color camera

no debayering


### Readout format

### Hot pixel detection

combined with binning

### Auto exposure




## Speed vs accuracy slider

auto exposure vs manual

## Motion analysis

detect tracking eq vs. altaz
detect dwell
  - turn down frame rate after delay
  - auto-create journal entry
declination drift estimate for polar alignment

## Polar alignment

## Auto-adjustment of frame rate

## Catalog integration

## SkySafari integration

