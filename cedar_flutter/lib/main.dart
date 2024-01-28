import 'dart:developer';
import 'dart:math' as math;
import 'package:fixnum/fixnum.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'package:grpc/service_api.dart';
import 'package:numberpicker/numberpicker.dart';
import 'package:sprintf/sprintf.dart';
import 'cedar.pbgrpc.dart';
import 'tetra3.pb.dart';
import 'google/protobuf/duration.pb.dart' as proto_duration;
import 'get_cedar_client_for_web.dart'
    if (dart.library.io) 'get_cedar_client.dart';

// To generate release build: flutter build web

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
        brightness: Brightness.dark,
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
    if (state._setupMode) {
      canvas.drawRect(
          state._centerRegion,
          Paint()
            ..color = Colors.red
            ..strokeWidth = thick
            ..style = PaintingStyle.stroke);
      // Draw box around location of the brightest star in search box.
      canvas.drawRect(
          state._centerPeakRegion,
          Paint()
            ..color = Colors.red
            ..strokeWidth = thin
            ..style = PaintingStyle.stroke);
      for (var star in state._stars) {
        var offset = Offset(star.centroidPosition.x / state._binFactor,
            star.centroidPosition.y / state._binFactor);
        canvas.drawCircle(
            offset,
            4,
            Paint()
              ..color = Colors.red
              ..strokeWidth = hairline
              ..style = PaintingStyle.stroke);
      }
    }
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) {
    return true;
  }
}

class _MyHomePageState extends State<MyHomePage> {
  // Information from most recent FrameResult.

  // Image data, binned by server.
  Uint8List _imageBytes = Uint8List(1);
  int _binFactor = 1;

  bool _setupMode = false;
  int _accuracy = 3; // 1-4.

  Offset? _boresightPosition; // Scaled by main image's binning.

  late Rect _centerRegion; // Scaled by main image's binning.
  late Rect _centerPeakRegion; // Scaled by binning.

  int _centerPeakWidth = 0;
  int _centerPeakHeight = 0;
  Uint8List _centerPeakImageBytes = Uint8List(1);

  int _prevFrameId = -1;
  late List<StarCentroid> _stars;
  int _numStars = 0;
  double _exposureTimeMs = 0.0;
  bool _hasSolution = false;

  // Degrees.
  double _solutionRA = 0.0;
  double _solutionDec = 0.0;

  // Arcsec.
  double _solutionRMSE = 0.0;

  // Values set from on-screen controls.
  bool _doRefreshes = false;
  int _expSettingMs = 0; // 0 is auto-exposure.

  CedarClient? _client;
  CedarClient client() {
    _client ??= getClient(); // Initialize if null.
    return _client!;
  }

  void setStateFromFrameResult(FrameResult response) {
    _prevFrameId = response.frameId;
    _stars = response.starCandidates;
    _numStars = _stars.length;
    _hasSolution = false;
    _accuracy = response.operationSettings.accuracy.toInt();
    if (response.hasPlateSolution()) {
      SolveResult plateSolution = response.plateSolution;
      if (!plateSolution.hasFailureReason()) {
        _hasSolution = true;
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
      _binFactor = response.image.binningFactor;
    }
    if (response.hasBoresightPosition()) {
      _boresightPosition = Offset(response.boresightPosition.x / _binFactor,
          response.boresightPosition.y / _binFactor);
    } else {
      _boresightPosition = null;
    }
    if (response.hasCenterRegion()) {
      _setupMode = true;
      var cr = response.centerRegion;
      _centerRegion = Rect.fromLTWH(
          cr.originX.toDouble() / _binFactor,
          cr.originY.toDouble() / _binFactor,
          cr.width.toDouble() / _binFactor,
          cr.height.toDouble() / _binFactor);
    } else {
      _setupMode = false;
    }
    if (response.hasExposureTime()) {
      _exposureTimeMs = durationToMs(response.exposureTime);
    }
    _expSettingMs =
        durationToMs(response.operationSettings.exposureTime).toInt();
    if (response.hasCenterPeakImage()) {
      _centerPeakImageBytes =
          Uint8List.fromList(response.centerPeakImage.imageData);
      _centerPeakWidth = response.centerPeakImage.rectangle.width;
      _centerPeakHeight = response.centerPeakImage.rectangle.height;
    }
    if (response.hasCenterPeakPosition()) {
      var cp = response.centerPeakPosition;
      _centerPeakRegion = Rect.fromCenter(
          center: Offset(cp.x / _binFactor, cp.y / _binFactor),
          width: _centerPeakWidth.toDouble() / _binFactor,
          height: _centerPeakHeight.toDouble() / _binFactor);
    }
  }

  Future<void> updateOperationSettings(OperationSettings request) async {
    try {
      await client().updateOperationSettings(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
    } catch (e) {
      log('Error: $e');
    }
  }

  // Use request/response style of RPC.
  Future<void> getFrameFromServer() async {
    final request = FrameRequest()
      ..prevFrameId = _prevFrameId
      ..mainImageMode = ImageMode.BINNED;
    try {
      final response = await client().getFrame(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
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
      await Future.delayed(const Duration(milliseconds: 10));
      await getFrameFromServer();
      return _doRefreshes;
    });
  }

  Future<void> initiateAction(ActionRequest request) async {
    try {
      await client().initiateAction(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
    } catch (e) {
      log('Error: $e');
    }
  }

  Future<void> setExpTime() async {
    var request = OperationSettings();
    request.exposureTime = msToDuration(_expSettingMs);
    await updateOperationSettings(request);
  }

  Future<void> captureBoresight() async {
    var request = ActionRequest();
    request.captureBoresight = true;
    await initiateAction(request);
  }

  Future<void> setOperatingMode(bool setup) async {
    var request = OperationSettings();
    request.operatingMode = setup ? OperatingMode.SETUP : OperatingMode.OPERATE;
    await updateOperationSettings(request);
  }

  Future<void> setAccuracy(int value) async {
    var request = OperationSettings();
    request.accuracy = value;
    await updateOperationSettings(request);
  }

  void shutdownDialog() {
    showDialog(
      context: context,
      barrierDismissible: false,
      builder: (BuildContext context) {
        return AlertDialog(
          content: const Text('Shutdown Raspberry Pi?'),
          actions: <Widget>[
            TextButton(
                child: const Text('Shutdown'),
                onPressed: () {
                  shutdown();
                  Navigator.of(context).pop();
                }),
            TextButton(
                child: const Text('Cancel'),
                onPressed: () {
                  Navigator.of(context).pop();
                }),
          ],
        );
      },
    );
  }

  Future<void> shutdown() async {
    var request = ActionRequest();
    request.shutdownServer = true;
    await initiateAction(request);
  }

  Future<void> saveImage() async {
    var request = ActionRequest();
    request.saveImage = true;
    await initiateAction(request);
  }

  Widget runSwitch() {
    return Switch(
        value: _doRefreshes,
        onChanged: (bool value) {
          setState(() {
            _doRefreshes = value;
            if (_doRefreshes) {
              refreshStateFromServer();
            }
          });
        }); // Switch
  }

  Color starsSliderColor() {
    return _hasSolution ? const Color(0xff00c000) : const Color(0xff606060);
  }

  List<Widget> controls() {
    return <Widget>[
      Column(
        children: <Widget>[
          runSwitch(),
          const Text("Run"),
        ],
      ),
      Column(
        children: <Widget>[
          const Text("Stars detected"),
          const Text(" 0       6      25     55    100"),
          Slider(
            min: 0,
            max: 10,
            value: math.min(10, math.sqrt(_numStars)),
            onChanged: (double value) {},
            activeColor: starsSliderColor(),
            thumbColor: starsSliderColor(),
          ),
        ],
      ),
      _setupMode
          ? const SizedBox(height: 2)
          : Column(
              children: <Widget>[
                Text(sprintf("%.4f", [_solutionRA])),
                Text(sprintf("%.4f", [_solutionDec])),
                Text(sprintf("%.2f", [_solutionRMSE])),
                Container(
                  margin: const EdgeInsets.all(10),
                  child: const Text("RA/DEC/RMSE"),
                ),
              ],
            ),
      _setupMode
          ? const SizedBox(height: 2)
          : Column(
              children: <Widget>[
                const Text("Fast              Accurate"),
                Slider(
                  min: 1,
                  max: 4,
                  value: _accuracy.toDouble(),
                  onChanged: (double value) {
                    setAccuracy(value.toInt());
                  },
                ),
              ],
            ),
      Column(
        children: <Widget>[
          NumberPicker(
              axis: Axis.horizontal,
              itemWidth: 40,
              itemHeight: 30,
              minValue: 0,
              maxValue: 200,
              step: 10,
              value: _expSettingMs,
              onChanged: (value) => {
                    setState(() {
                      _expSettingMs = value;
                      setExpTime();
                    })
                  }),
          Text(sprintf("Exp time %.1f", [_exposureTimeMs])),
        ],
      ),
      Column(children: <Widget>[
        _setupMode
            ? OutlinedButton(
                child: const Text("Exit setup"),
                onPressed: () {
                  setOperatingMode(/*setup=*/ false);
                })
            : OutlinedButton(
                child: const Text("Setup"),
                onPressed: () {
                  setOperatingMode(/*setup=*/ true);
                })
      ]),
      _setupMode
          ? Column(children: <Widget>[
              OutlinedButton(
                  child: const Text("Set alignment"),
                  onPressed: () {
                    captureBoresight();
                  }),
            ])
          : const SizedBox(height: 2),
      Column(children: <Widget>[
        OutlinedButton(
            child: const Text("Save image"),
            onPressed: () {
              saveImage();
            }),
      ]),
      Column(children: <Widget>[
        OutlinedButton(
            child: const Text("Shutdown"),
            onPressed: () {
              shutdownDialog();
            }),
      ]),
    ];
  }

  Widget mainImage() {
    return CustomPaint(
      foregroundPainter: _MainImagePainter(this),
      child: dart_widgets.Image.memory(_imageBytes, gaplessPlayback: true),
    );
  }

  Widget imageStack() {
    return Stack(
      alignment: Alignment.topRight,
      children: <Widget>[
        _prevFrameId != -1 ? mainImage() : const SizedBox(height: 2),
        _prevFrameId != -1 && _setupMode
            ? dart_widgets.Image.memory(_centerPeakImageBytes,
                height: _centerPeakHeight.toDouble() * 3,
                width: _centerPeakWidth.toDouble() * 3,
                fit: BoxFit.fill,
                gaplessPlayback: true)
            : const SizedBox(height: 2),
      ],
    );
  }

  Widget orientationLayout(BuildContext context) {
    if (MediaQuery.of(context).orientation == Orientation.portrait) {
      return Column(
        children: <Widget>[
          Row(children: controls()),
          imageStack(),
        ],
      );
    } else {
      // Landscape
      return Row(
        children: <Widget>[
          imageStack(),
          Column(children: controls()),
        ],
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    goFullScreen();
    // This method is rerun every time setState() is called.
    return Scaffold(body: FittedBox(child: orientationLayout(context)));
  }
}
