import 'dart:developer';
import 'dart:math' as math;
import 'dart:typed_data';
import 'package:fixnum/fixnum.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'package:grpc/service_api.dart';
import 'package:sprintf/sprintf.dart';
import 'cedar.pbgrpc.dart';
import 'tetra3.pb.dart';
import 'google/protobuf/duration.pb.dart' as proto_duration;
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

double durationToMs(proto_duration.Duration duration) {
  return duration.seconds.toDouble() * 1000 +
      (duration.nanos.toDouble()) / 1000000;
}

proto_duration.Duration msToDuration(int ms) {
  var duration = proto_duration.Duration();
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
    // Draw search box within which we search for the brightest star for
    // focusing.
    canvas.drawRect(
        state._centerRegion,
        Paint()
          ..color = Colors.red
          ..strokeWidth = thick
          ..style = PaintingStyle.stroke);
    double crossRadius = state._boresightPosition == null ? 4 : 8;
    double crossThickness = state._boresightPosition == null ? hairline : thin;
    var center = state._boresightPosition ?? state._centerRegion.center;
    // Make a cross at the boresight position (if any) or else the center of
    // the search box (which is overall image center.
    canvas.drawLine(
        center.translate(-crossRadius, 0),
        center.translate(crossRadius, 0),
        Paint()
          ..color = Colors.red
          ..strokeWidth = crossThickness);
    canvas.drawLine(
        center.translate(0, -crossRadius),
        center.translate(0, crossRadius),
        Paint()
          ..color = Colors.red
          ..strokeWidth = crossThickness);
    // Draw box around location of the brightest star in search box.
    canvas.drawRect(
        state._centerPeakRegion,
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
  Uint8List _imageBytes = Uint8List(1);
  int _width = 0;
  int _height = 0;

  Offset? _boresightPosition; // Scaled by main image's binning.

  late Rect _centerRegion; // Scaled by main image's binning.
  late Rect _centerPeakRegion; // Scaled by binning.

  int _centerPeakWidth = 0;
  int _centerPeakHeight = 0;
  Uint8List _centerPeakImageBytes = Uint8List(1);

  int _prevFrameId = -1;
  int _numStarCandidates = 0;
  double _exposureTimeMs = 0.0;
  String _solveFailureReason = "";

  // Degrees.
  double _solutionRA = 0.0;
  double _solutionDec = 0.0;

  // Arcsec.
  double _solutionRMSE = 0.0;

  // Values set from on-screen controls.
  bool _doRefreshes = false;
  bool _expAuto = true;
  int _expSliderValue = 0;

  CedarClient? _client;
  CedarClient client() {
    _client ??= getClient(); // Initialize if null.
    return _client!;
  }

  bool hasSolution() {
    return _solveFailureReason == "";
  }

  void setStateFromFrameResult(FrameResult response) {
    // TODO(smr): check response.operatingMode and extract information
    // accordingly. Also render widgets according to the operatingMode.
    _prevFrameId = response.frameId;
    _numStarCandidates = response.starCandidates.length;
    int binFactor = 1;
    if (response.hasPlateSolution()) {
      SolveResult plateSolution = response.plateSolution;
      if (plateSolution.hasFailureReason()) {
        _solveFailureReason = plateSolution.failureReason;
      } else {
        _solveFailureReason = "";
        if (plateSolution.targetCoords.isNotEmpty) {
          _solutionRA = plateSolution.targetCoords.first.ra;
          _solutionDec = plateSolution.targetCoords.first.dec;
        } else {
          _solutionRA = plateSolution.imageCenterCoords.ra;
          _solutionDec = plateSolution.imageCenterCoords.dec;
        }
        _solutionRMSE = plateSolution.rmse;
      }
    }
    if (response.hasImage()) {
      _imageBytes = Uint8List.fromList(response.image.imageData);
      _width = response.image.rectangle.width;
      _height = response.image.rectangle.height;
      binFactor = response.image.binningFactor;
    }
    if (response.hasBoresightPosition()) {
      _boresightPosition = Offset(response.boresightPosition.x / binFactor,
          response.boresightPosition.y / binFactor);
    } else {
      _boresightPosition = null;
    }
    if (response.hasCenterRegion()) {
      var cr = response.centerRegion;
      _centerRegion = Rect.fromLTWH(
          cr.originX.toDouble() / binFactor,
          cr.originY.toDouble() / binFactor,
          cr.width.toDouble() / binFactor,
          cr.height.toDouble() / binFactor);
    }
    if (response.hasExposureTime()) {
      _exposureTimeMs = durationToMs(response.exposureTime);
    }
    _expAuto = durationToMs(response.operationSettings.exposureTime) == 0.0;
    _centerPeakImageBytes =
        Uint8List.fromList(response.centerPeakImage.imageData);
    _centerPeakWidth = response.centerPeakImage.rectangle.width;
    _centerPeakHeight = response.centerPeakImage.rectangle.height;
    if (response.hasCenterPeakPosition()) {
      var cp = response.centerPeakPosition;
      _centerPeakRegion = Rect.fromCenter(
          center: Offset(cp.x / binFactor, cp.y / binFactor),
          width: _centerPeakWidth.toDouble() / binFactor,
          height: _centerPeakHeight.toDouble() / binFactor);
    }
  }

  Future<void> updateOperationSettings(OperationSettings request) async {
    try {
      await client().updateOperationSettings(request,
          options: CallOptions(timeout: const Duration(seconds: 1)));
    } catch (e) {
      log('Error: $e');
    }
  }

  // Use request/response style of RPC.
  Future<void> getFocusFrameFromServer() async {
    final request = FrameRequest()
      ..prevFrameId = _prevFrameId
      ..mainImageMode = ImageMode.IMAGE_MODE_BINNED;
    try {
      final response = await client().getFrame(request,
          options: CallOptions(timeout: const Duration(seconds: 1)));
      setState(() {
        setStateFromFrameResult(response);
      });
    } catch (e) {
      log('Error: $e');
    }
  }

  // Issue repeated request/response RPCs.
  Future<void> refreshStateFromServer() async {
    await Future.doWhile(() async {
      await getFocusFrameFromServer();
      return _doRefreshes;
    });
  }

  // Issue streaming RPC. Alternative to refreshStateFromServer().
  Future<void> refreshStateFromStreamingServer() async {
    final request = FrameRequest()
      ..prevFrameId = _prevFrameId
      ..mainImageMode = ImageMode.IMAGE_MODE_BINNED;
    // We wrap the getFrames() streaming RPC call in a loop because it can
    // fail for various spurious causes; we just restart it.
    await Future.doWhile(() async {
      try {
        await for (var response in client().getFrames(request)) {
          setState(() {
            setStateFromFrameResult(response);
          });
          if (!_doRefreshes) {
            break;
          }
        }
      } catch (e) {
        log('Error: $e');
      }
      return _doRefreshes;
    });
  }

  Future<void> initiateAction(ActionRequest request) async {
    try {
      await client().initiateAction(request,
          options: CallOptions(timeout: const Duration(seconds: 1)));
    } catch (e) {
      log('Error: $e');
    }
  }

  Future<void> setExpTimeFromSlider() async {
    var request = OperationSettings();
    request.exposureTime = msToDuration(expValuesMs[_expSliderValue]);
    await updateOperationSettings(request);
  }

  Future<void> captureBoresight() async {
    var request = ActionRequest();
    request.captureBoresight = true;
    await initiateAction(request);
  }

  Widget runSwitch() {
    return Switch(
        value: _doRefreshes,
        onChanged: (bool value) {
          setState(() {
            _doRefreshes = value;
            if (_doRefreshes) {
              // We prefer the request/response style of RPC. The streaming
              // RPC sometimes seems to introduce more lag (buffering?), but
              // not really sure. The request/response style seems fine, so
              // we'll stick with it.
              refreshStateFromServer();
              // refreshStateFromStreamingServer();
            }
          });
        }); // Switch
  }

  Widget expControl() {
    return Column(children: <Widget>[
      _expAuto
          ? const SizedBox(height: 48)
          : Slider(
              min: 0,
              max: expValuesMs.length - 1,
              divisions: expValuesMs.length - 1,
              value: expValueIndex(_exposureTimeMs).toDouble(),
              onChanged: (double value) => {
                    setState(() {
                      _expSliderValue = value.toInt();
                      if (!_expAuto) {
                        setExpTimeFromSlider();
                      }
                    })
                  }),
      Row(
        children: <Widget>[
          Switch(
              value: _expAuto,
              onChanged: (bool value) => {
                    setState(() {
                      _expAuto = value;
                      if (_expAuto) {
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
      Text("$_exposureTimeMs"),
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
              value: math.min(10, math.sqrt(_numStarCandidates)),
              onChanged: (double value) {},
              activeColor: hasSolution() ? Colors.green : Colors.grey,
              thumbColor: hasSolution() ? Colors.green : Colors.grey,
            ),
            const Text("Stars detected"),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.4f", [_solutionRA])),
            Container(
              margin: const EdgeInsets.all(10),
              child: const Text("     RA     "),
            ),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.4f", [_solutionDec])),
            Container(
              margin: const EdgeInsets.all(10),
              child: const Text("    DEC    "),
            ),
          ],
        ),
        Column(
          children: <Widget>[
            Text(sprintf("%.2f", [_solutionRMSE])),
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
        Column(children: <Widget>[
          TextButton(
              child: const Text("Capture boresight"),
              onPressed: () {
                captureBoresight();
              }),
        ]),
      ],
    );
  }

  Widget mainImage() {
    return CustomPaint(
      foregroundPainter: _MainImagePainter(this),
      child: dart_widgets.Image.memory(_imageBytes,
          height: _height.toDouble() / 2,
          width: _width.toDouble() / 2,
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
                _prevFrameId != -1 ? mainImage() : const SizedBox(height: 2),
                _prevFrameId != -1
                    ? dart_widgets.Image.memory(_centerPeakImageBytes,
                        height: _centerPeakHeight.toDouble() * 3,
                        width: _centerPeakWidth.toDouble() * 3,
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
