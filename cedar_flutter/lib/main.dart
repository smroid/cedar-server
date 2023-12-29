import 'dart:developer';
import 'dart:math' as math;
import 'dart:typed_data';
import 'package:fixnum/fixnum.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'package:sprintf/sprintf.dart';
import 'cedar.pbgrpc.dart';
import 'tetra3.pb.dart';
import 'google/protobuf/duration.pb.dart';
import 'get_cedar_client_for_web.dart'
    if (dart.library.io) 'get_cedar_client.dart';

void main() {
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  // This widget is the root of your application.
  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Cedar',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.deepPurple),
        useMaterial3: true,
      ),
      home: const MyHomePage(title: 'Cedar'),
    );
  }
}

class MyHomePage extends StatefulWidget {
  const MyHomePage({super.key, required this.title});
  final String title;
  @override
  State<MyHomePage> createState() => _MyHomePageState();
}

double durationToMs(Duration duration) {
  return duration.seconds.toDouble() * 1000 +
      (duration.nanos.toDouble()) / 1000000;
}

Duration msToDuration(int ms) {
  var duration = Duration();
  duration.seconds = Int64(ms ~/ 1000);
  duration.nanos = (ms * 1000000) % 1000000000;
  return duration;
}

// The various exposure times (ms) selected by the exposure time slider.
// TODO: build from the min/max exposure times in the CalibrationData.
var expValuesMs = [10, 20, 50, 100, 200, 500, 1000];

// Return the largest index in expValuesMs array that is <= the given value.
// If the given value is too small returns 0.
int expValueIndex(double value) {
  if (value <= expValuesMs[0]) {
    return 0;
  }
  int index = 0;
  while (++index < expValuesMs.length) {
    if (expValuesMs[index] > value) {
      return index - 1;
    }
  }
  return expValuesMs.length - 1;
}

class _MainImagePainter extends CustomPainter {
  final _MyHomePageState state;

  _MainImagePainter(this.state);

  @override
  void paint(Canvas canvas, Size size) {
    const double hairline = 0.5;
    const double thin = 1;
    const double thick = 2;
    const double crossRadius = 4;
    // Draw search box within which we search for the brightest star for
    // focusing.
    canvas.drawRect(
        state.centerRegion,
        Paint()
          ..color = Colors.red
          ..strokeWidth = thick
          ..style = PaintingStyle.stroke);
    // Make a cross at the center of the search box (which is overall image
    // center. TODO: draw the cross at the boresight center, if present.
    var center = state.centerRegion.center;
    canvas.drawLine(
        center.translate(-crossRadius, 0),
        center.translate(crossRadius, 0),
        Paint()
          ..color = Colors.red
          ..strokeWidth = hairline);
    canvas.drawLine(
        center.translate(0, -crossRadius),
        center.translate(0, crossRadius),
        Paint()
          ..color = Colors.red
          ..strokeWidth = hairline);
    // Draw box around location of the brightest star in search box.
    canvas.drawRect(
        state.centerPeakRegion,
        Paint()
          ..color = Colors.red
          ..strokeWidth = thin
          ..style = PaintingStyle.stroke);
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) {
    return true;
  }
}

class _MyHomePageState extends State<MyHomePage> {
  // Information from most recent FrameResult.
  Uint8List imageBytes = Uint8List(1);
  int width = 0;
  int height = 0;

  late Rect centerRegion; // Scaled by main image's binning.
  late Rect centerPeakRegion; // Scaled by binning.

  int centerPeakWidth = 0;
  int centerPeakHeight = 0;
  Uint8List centerPeakImageBytes = Uint8List(1);

  int prevFrameId = -1;
  int numStarCandidates = 0;
  double exposureTimeMs = 0.0;
  String solveFailureReason = "";
  double solutionRA = 0.0;
  double solutionDec = 0.0;
  double solutionRMSE = 0.0;

  // Values set from on-screen controls.
  bool doRefreshes = false;
  bool expAuto = true;
  int expSliderValue = 0;

  bool hasSolution() {
    return solveFailureReason == "";
  }

  void setStateFromFrameResult(FrameResult response) {
    // TODO(smr): check response.operatingMode and extract information
    // accordingly. Also render widgets according to the operatingMode.
    prevFrameId = response.frameId;
    numStarCandidates = response.starCandidates.length;
    int binFactor = 1;
    if (response.hasPlateSolution()) {
      SolveResult plateSolution = response.plateSolution;
      if (plateSolution.hasFailureReason()) {
        solveFailureReason = plateSolution.failureReason;
      } else {
        solveFailureReason = "";
        solutionRA = plateSolution.imageCenterCoords.ra;
        solutionDec = plateSolution.imageCenterCoords.dec;
        solutionRMSE = plateSolution.rmse;
      }
    }
    if (response.hasImage()) {
      imageBytes = Uint8List.fromList(response.image.imageData);
      width = response.image.rectangle.width;
      height = response.image.rectangle.height;
      binFactor = response.image.binningFactor;
    }
    if (response.hasCenterRegion()) {
      var cr = response.centerRegion;
      centerRegion = Rect.fromLTWH(
          cr.originX.toDouble() / binFactor,
          cr.originY.toDouble() / binFactor,
          cr.width.toDouble() / binFactor,
          cr.height.toDouble() / binFactor);
    }
    if (response.hasExposureTime()) {
      exposureTimeMs = durationToMs(response.exposureTime);
    }
    expAuto = durationToMs(response.operationSettings.exposureTime) == 0.0;
    centerPeakImageBytes =
        Uint8List.fromList(response.centerPeakImage.imageData);
    centerPeakWidth = response.centerPeakImage.rectangle.width;
    centerPeakHeight = response.centerPeakImage.rectangle.height;
    if (response.hasCenterPeakPosition()) {
      var cp = response.centerPeakPosition;
      centerPeakRegion = Rect.fromCenter(
          center: Offset(cp.x / binFactor, cp.y / binFactor),
          width: centerPeakWidth.toDouble() / binFactor,
          height: centerPeakHeight.toDouble() / binFactor);
    }
  }

  Future<void> getFocusFrameFromServer() async {
    final CedarClient client = getClient();

    final request = FrameRequest()
      ..prevFrameId = prevFrameId
      ..mainImageMode = ImageMode.IMAGE_MODE_BINNED;
    try {
      final response = await client.getFrame(request);
      setState(() {
        setStateFromFrameResult(response);
      });
    } catch (e) {
      log('Error: $e');
    }
  }

  Future<void> refreshStateFromServer() async {
    await Future.doWhile(() async {
      await getFocusFrameFromServer();
      return doRefreshes;
    });
  }

  Future<void> updateOperationSettings(OperationSettings request) async {
    final CedarClient client = getClient();
    try {
      await client.updateOperationSettings(request);
    } catch (e) {
      log('Error: $e');
    }
  }

  Future<void> setExpTimeFromSlider() async {
    var request = OperationSettings();
    request.exposureTime = msToDuration(expValuesMs[expSliderValue]);
    await updateOperationSettings(request);
  }

  Widget runSwitch() {
    return Switch(
        value: doRefreshes,
        onChanged: (bool value) {
          setState(() {
            doRefreshes = value;
            if (doRefreshes) {
              refreshStateFromServer();
            }
          });
        }); // Switch
  }

  Widget expControl() {
    return Column(children: <Widget>[
      expAuto
          ? const SizedBox(height: 48)
          : Slider(
              min: 0,
              max: expValuesMs.length - 1,
              divisions: expValuesMs.length - 1,
              value: expValueIndex(exposureTimeMs).toDouble(),
              onChanged: (double value) => {
                    setState(() {
                      expSliderValue = value.toInt();
                      if (!expAuto) {
                        setExpTimeFromSlider();
                      }
                    })
                  }),
      Row(
        children: <Widget>[
          Switch(
              value: expAuto,
              onChanged: (bool value) => {
                    setState(() {
                      expAuto = value;
                      if (expAuto) {
                        var request = OperationSettings();
                        request.exposureTime = msToDuration(0);
                        updateOperationSettings(request);
                      } else {
                        setExpTimeFromSlider();
                      }
                    })
                  }),
          const Text("Auto"),
        ],
      ),
      Text("$exposureTimeMs"),
    ]);
  }

  Widget topControls() {
    return Row(
      children: <Widget>[
        Column(
          children: <Widget>[
            runSwitch(),
            const Text("Run"),
          ],
        ),
        Column(
          children: <Widget>[
            const Text(" 0       6      25     55    100"),
            Slider(
              min: 0,
              max: 10,
              value: math.min(10, math.sqrt(numStarCandidates)),
              onChanged: (double value) {},
              activeColor: hasSolution() ? Colors.green : Colors.grey,
              thumbColor: hasSolution() ? Colors.green : Colors.grey,
            ),
            const Text("Stars detected"),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.4f", [solutionRA])),
            Container(
              margin: const EdgeInsets.all(10),
              child: const Text("     RA     "),
            ),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.4f", [solutionDec])),
            Container(
              margin: const EdgeInsets.all(10),
              child: const Text("    DEC    "),
            ),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.2f", [solutionRMSE])),
            Container(
              margin: const EdgeInsets.all(10),
              child: const Text("RMSE"),
            ),
          ],
        ),
        Column(
          children: <Widget>[
            expControl(),
            const Text("Exp time (ms)"),
          ],
        ),
      ],
    );
  }

  Widget mainImage() {
    return CustomPaint(
      foregroundPainter: _MainImagePainter(this),
      child: dart_widgets.Image.memory(imageBytes,
          height: height.toDouble() / 2,
          width: width.toDouble() / 2,
          fit: BoxFit.none,
          gaplessPlayback: true),
    );
  }

  @override
  Widget build(BuildContext context) {
    // This method is rerun every time setState() is called.
    //
    // The Flutter framework has been optimized to make rerunning build methods
    // fast, so that you can just rebuild anything that needs updating rather
    // than having to individually change instances of widgets.
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        // Here we take the value from the MyHomePage object that was created by
        // the App.build method, and use it to set our appbar title.
        title: Text(widget.title),
      ),
      body: Center(
        // Center is a layout widget. It takes a single child and positions it
        // in the middle of the parent.
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: <Widget>[
            topControls(),
            const SizedBox(height: 2),
            Stack(
              alignment: Alignment.topRight,
              children: <Widget>[
                prevFrameId != -1 ? mainImage() : const SizedBox(height: 2),
                prevFrameId != -1
                    ? dart_widgets.Image.memory(centerPeakImageBytes,
                        height: centerPeakHeight.toDouble() * 3,
                        width: centerPeakWidth.toDouble() * 3,
                        fit: BoxFit.fill,
                        gaplessPlayback: true)
                    : const SizedBox(height: 2),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
