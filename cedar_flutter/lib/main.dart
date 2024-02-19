import 'dart:developer';
import 'dart:math' as math;
import 'package:cedar_flutter/draw_slew_target.dart';
import 'package:cedar_flutter/draw_util.dart';
import 'package:cedar_flutter/settings.dart';
import 'package:cedar_flutter/themes.dart';
import 'package:fixnum/fixnum.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'package:grpc/service_api.dart';
import 'package:numberpicker/numberpicker.dart';
import 'package:protobuf/protobuf.dart';
import 'package:provider/provider.dart';
import 'package:sprintf/sprintf.dart';
import 'cedar.pbgrpc.dart';
import 'tetra3.pb.dart';
import 'google/protobuf/duration.pb.dart' as proto_duration;
import 'get_cedar_client_for_web.dart'
    if (dart.library.io) 'get_cedar_client.dart';

// To generate release build: flutter build web

void main() {
  runApp(MultiProvider(
    providers: [
      ChangeNotifierProvider(create: (context) => SettingsModel()),
      ChangeNotifierProvider(create: (context) => ThemeModel()),
    ],
    child: const MyApp(),
  ));
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  // This widget is the root of your application.
  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Cedar Aim',
      theme: Provider.of<ThemeModel>(context).currentTheme,
      home: const MyHomePage(title: 'Cedar Aim'),
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
    if (state._setupMode) {
      // Draw search box within which we search for the brightest star for
      // focusing.
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
      // Draw circles around the detected stars.
      for (var star in state._stars) {
        var offset = Offset(star.centroidPosition.x / state._binFactor,
            star.centroidPosition.y / state._binFactor);
        canvas.drawCircle(
            offset,
            3,
            Paint()
              ..color = Colors.red
              ..strokeWidth = hairline
              ..style = PaintingStyle.stroke);
      }
    }

    var center = state._boresightPosition ?? state._imageRegion.center;
    if (state._slewRequest != null && !state._setupMode && state._hasSolution) {
      var slew = state._slewRequest;
      Offset? posInImage;
      if (slew!.hasImagePos()) {
        posInImage = Offset(slew.imagePos.x / state._binFactor,
            slew.imagePos.y / state._binFactor);
      }
      // How many display pixels is the telescope FOV?
      final scopeFov = state._preferences!.slewBullseyeSize *
          state._imageRegion.width /
          state._solutionFOV;
      drawSlewTarget(canvas, center, scopeFov, posInImage, slew.targetDistance,
          slew.targetAngle);
    } else {
      // Make a cross at the boresight position (if any) or else the image
      // center.
      double crossRadius = state._boresightPosition == null ? 4 : 8;
      double crossThickness =
          state._boresightPosition == null ? hairline : thin;
      drawCross(canvas, center, crossRadius, crossThickness);
    }
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) {
    return true;
  }
}

class _MyHomePageState extends State<MyHomePage> {
  _MyHomePageState() {
    refreshStateFromServer();
  }

  // Information from most recent FrameResult.

  // Image data, binned by server.
  Uint8List _imageBytes = Uint8List(1);
  late Rect _imageRegion; // Scaled by _binFactor.
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

  List<MatchedStar>? _solutionMatches;
  List<Offset>? _solutionCentroids;
  // Degrees.
  double _solutionRA = 0.0;
  double _solutionDec = 0.0;
  double _solutionRoll = 0.0;
  double _solutionFOV = 0.0;

  // Arcsec.
  double _solutionRMSE = 0.0;

  CalibrationData? _calibrationData;
  ProcessingStats? _processingStats;
  SlewRequest? _slewRequest;
  Preferences? _preferences;

  // Calibration happens when _setupMode transitions to false.
  bool _calibrating = false;
  double _calibrationProgress = 0.7;

  // Values set from on-screen controls.
  bool _doRefreshes = true;
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
    _calibrating = response.calibrating;
    if (response.calibrating) {
      _calibrationProgress = response.calibrationProgress;
    }
    if (_preferences == null ||
        _preferences?.nightVisionTheme !=
            response.preferences.nightVisionTheme) {
      if (response.preferences.nightVisionTheme) {
        Provider.of<ThemeModel>(context, listen: false).setNightVisionTheme();
      } else {
        Provider.of<ThemeModel>(context, listen: false).setNormalTheme();
      }
    }
    _preferences = response.preferences;
    Provider.of<SettingsModel>(context, listen: false).preferencesProto =
        _preferences!.deepCopy();
    _accuracy = response.operationSettings.accuracy.value;
    _expSettingMs =
        durationToMs(response.operationSettings.exposureTime).toInt();
    _setupMode =
        response.operationSettings.operatingMode == OperatingMode.SETUP;
    _calibrationData =
        response.hasCalibrationData() ? response.calibrationData : null;
    _processingStats =
        response.hasProcessingStats() ? response.processingStats : null;
    _slewRequest = response.hasSlewRequest() ? response.slewRequest : null;
    if (response.hasPlateSolution()) {
      SolveResult plateSolution = response.plateSolution;
      if (plateSolution.status == SolveStatus.MATCH_FOUND) {
        _hasSolution = true;
        _solutionMatches = plateSolution.matchedStars;
        _solutionCentroids = <Offset>[];
        for (var centroid in plateSolution.patternCentroids) {
          _solutionCentroids!.add(Offset(centroid.x, centroid.y));
        }
        if (plateSolution.targetCoords.isNotEmpty) {
          _solutionRA = plateSolution.targetCoords.first.ra;
          _solutionDec = plateSolution.targetCoords.first.dec;
        } else {
          _solutionRA = plateSolution.imageCenterCoords.ra;
          _solutionDec = plateSolution.imageCenterCoords.dec;
        }
        _solutionRoll = plateSolution.roll;
        _solutionRMSE = plateSolution.rmse;
        _solutionFOV = plateSolution.fov;
      }
    }
    _imageBytes = Uint8List.fromList(response.image.imageData);
    _binFactor = response.image.binningFactor;
    _imageRegion = Rect.fromLTWH(
        0,
        0,
        response.image.rectangle.width.toDouble() / _binFactor,
        response.image.rectangle.height.toDouble() / _binFactor);
    if (response.hasBoresightPosition()) {
      _boresightPosition = Offset(response.boresightPosition.x / _binFactor,
          response.boresightPosition.y / _binFactor);
    } else {
      _boresightPosition = null;
    }
    if (response.hasCenterRegion()) {
      var cr = response.centerRegion;
      _centerRegion = Rect.fromLTWH(
          cr.originX.toDouble() / _binFactor,
          cr.originY.toDouble() / _binFactor,
          cr.width.toDouble() / _binFactor,
          cr.height.toDouble() / _binFactor);
    }
    if (response.hasExposureTime()) {
      _exposureTimeMs = durationToMs(response.exposureTime);
    }
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
      var delay = _calibrating || !_doRefreshes ? 100 : 10;
      await Future.delayed(Duration(milliseconds: delay));
      if (_doRefreshes) {
        await getFrameFromServer();
      }
      return true; // Forever!
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

  Future<void> stopSlew() async {
    var request = ActionRequest();
    request.stopSlew = true;
    await initiateAction(request);
  }

  Future<void> setOperatingMode(bool setup) async {
    var request = OperationSettings();
    request.operatingMode = setup ? OperatingMode.SETUP : OperatingMode.OPERATE;
    await updateOperationSettings(request);
  }

  Future<void> setAccuracy(int value) async {
    var request = OperationSettings();
    request.accuracy = Accuracy.valueOf(value)!;
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

  Future<void> cancelCalibration() async {
    await setOperatingMode(/*setup=*/ true);
  }

  Future<void> updatePreferences(Preferences prefs) async {
    try {
      await client().updatePreferences(prefs,
          options: CallOptions(timeout: const Duration(seconds: 10)));
    } catch (e) {
      log('Error: $e');
    }
  }

  List<Widget> drawerControls() {
    return <Widget>[
      CloseButton(
          style: ButtonStyle(
              alignment: Alignment.topLeft,
              iconColor: MaterialStatePropertyAll(
                  Theme.of(context).colorScheme.primary))),
      const SizedBox(height: 15),
      Column(
        children: <Widget>[
          const Text("Fast              Accurate"),
          Slider(
            min: 1,
            max: 4,
            value: _accuracy.toDouble(),
            onChanged: (double value) => {
              setState(() {
                _accuracy = value.toInt();
                setAccuracy(value.toInt());
              })
            },
          ),
        ],
      ),
      const SizedBox(height: 15),
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
          Text(sprintf("Exp time %.1f ms", [_exposureTimeMs])),
          const SizedBox(height: 15),
        ],
      ),
      const SizedBox(height: 15),
      Column(children: <Widget>[
        OutlinedButton(
            child: const Text("Save image"),
            onPressed: () {
              saveImage();
            }),
      ]),
      const SizedBox(height: 15),
      Column(children: <Widget>[
        OutlinedButton(
            child: const Text("Shutdown"),
            onPressed: () {
              shutdownDialog();
            }),
      ]),
      const SizedBox(height: 15),
      TextButton.icon(
          label: const Text("Preferences"),
          icon: const Icon(Icons.settings),
          onPressed: () {
            // Dismiss drawer, so when user exits out of settings we go
            // back to main display.
            Navigator.of(context).pop();
            Navigator.push(
                context,
                MaterialPageRoute(
                    builder: (context) => const SettingsScreen()));
          }),
    ];
  }

  List<Widget> controls() {
    return <Widget>[
      // Fake widget to consume changes to preferences and issue RPC to the
      // server.
      Consumer<SettingsModel>(
        builder: (context, settings, child) {
          final newPrefs = settings.preferencesProto;
          var prefsDiff = newPrefs.deepCopy();
          if (_preferences != null &&
              diffPreferences(_preferences!, prefsDiff)) {
            updatePreferences(prefsDiff);
            _preferences = newPrefs.deepCopy();
          }
          return Container();
        },
      ),
      Column(children: <Widget>[
        Row(children: <Widget>[
          primaryText("Setup"),
          Switch(
              value: !_setupMode,
              onChanged: (bool value) {
                setState(() {
                  setOperatingMode(/*setup=*/ !value);
                });
              }),
          primaryText("Run"),
        ])
      ]),
      const SizedBox(width: 15, height: 15),
      SizedBox(
          width: 140,
          height: 32,
          child: _setupMode
              ? Column(children: <Widget>[
                  OutlinedButton(
                      child: const Text("Set alignment"),
                      onPressed: () {
                        captureBoresight();
                      }),
                ])
              : _slewRequest != null && !_setupMode
                  ? Column(children: <Widget>[
                      OutlinedButton(
                          child: const Text("End goto"),
                          onPressed: () {
                            stopSlew();
                          }),
                    ])
                  : Container()),
    ];
  }

  String formatRightAscension(double ra) {
    if (_preferences?.celestialCoordFormat == CelestialCoordFormat.DECIMAL) {
      return sprintf("%.4f°", [ra]);
    }
    int hours = (ra / 15.0).floor();
    double fracHours = ra / 15.0 - hours;
    int minutes = (fracHours * 60.0).floor();
    double fracMinutes = fracHours * 60.0 - minutes;
    double seconds = fracMinutes * 60;
    return sprintf("%02dh %02dm %02.1fs", [hours, minutes, seconds]);
  }

  String formatDeclination(double dec) {
    if (_preferences?.celestialCoordFormat == CelestialCoordFormat.DECIMAL) {
      return sprintf("%.4f°", [dec]);
    }
    String sign = dec < 0 ? "-" : "+";
    if (dec < 0) {
      dec = -dec;
    }
    int degrees = dec.floor();
    double fracDegrees = dec - degrees;
    int minutes = (fracDegrees * 60.0).floor();
    double fracMinutes = fracDegrees * 60.0 - minutes;
    double seconds = fracMinutes * 60;
    return sprintf("%s%02d° %02d' %02.1f''", [sign, degrees, minutes, seconds]);
  }

  Color starsSliderColor() {
    return _hasSolution
        ? Theme.of(context).colorScheme.primary
        : const Color(0xff606060);
  }

  Color solveTextColor() {
    return _hasSolution
        ? Theme.of(context).colorScheme.primary
        : const Color(0xff606060);
  }

  Text primaryText(String val) {
    return Text(val,
        style: TextStyle(color: Theme.of(context).colorScheme.primary));
  }

  Text solveText(String val) {
    return Text(val, style: TextStyle(color: solveTextColor()));
  }

  List<Widget> dataItems(BuildContext context) {
    return <Widget>[
      Column(children: <Widget>[
        SizedBox(
            width: 130,
            height: 20,
            child: Slider(
              min: 0,
              max: 10,
              value: math.min(10, math.sqrt(_numStars)),
              onChanged: (double value) {},
              activeColor: starsSliderColor(),
              thumbColor: starsSliderColor(),
            )),
        primaryText("$_numStars stars"),
        const SizedBox(width: 15, height: 15),
        _calibrationData != null && _calibrationData!.fovHorizontal > 0
            ? Column(children: <Widget>[
                primaryText(
                    sprintf("FOV %.1f°", [_calibrationData!.fovHorizontal])),
                primaryText(
                    sprintf("Lens %.1f mm", [_calibrationData!.lensFlMm])),
              ])
            : Container(),
      ]),
      const SizedBox(width: 15, height: 15),
      _setupMode
          ? Container()
          : SizedBox(
              width: 140,
              height: 90,
              child: Column(
                children: <Widget>[
                  primaryText("Plate solution"),
                  solveText(
                      sprintf("RA %s", [formatRightAscension(_solutionRA)])),
                  solveText(
                      sprintf("Dec %s", [formatDeclination(_solutionDec)])),
                  solveText(sprintf(
                      "roll %.1f° err %.1f''", [_solutionRoll, _solutionRMSE])),
                ],
              )),
      const SizedBox(width: 15, height: 15),
      _slewRequest == null || _setupMode
          ? Container()
          : SizedBox(
              width: 140,
              height: 110,
              child: Column(children: <Widget>[
                primaryText("Goto target"),
                solveText(sprintf(
                    "RA %s", [formatRightAscension(_slewRequest!.target.ra)])),
                solveText(sprintf(
                    "Dec %s", [formatDeclination(_slewRequest!.target.dec)])),
                Column(children: <Widget>[
                  solveText(sprintf(
                      "distance %.4f°", [_slewRequest?.targetDistance])),
                  solveText(
                      sprintf("angle %.2f°", [_slewRequest?.targetAngle])),
                ])
              ]),
            ),
      const SizedBox(width: 15, height: 15),
      _setupMode || _processingStats == null || !_preferences!.showPerfStats
          ? Container()
          : Column(
              children: <Widget>[
                primaryText(sprintf("Solve interval  %.1f ms",
                    [_processingStats!.solveInterval.recent.mean * 1000])),
                primaryText(sprintf("Detect latency  %.1f ms",
                    [_processingStats!.detectLatency.recent.mean * 1000])),
                primaryText(sprintf("Solve latency  %.1f ms",
                    [_processingStats!.solveLatency.recent.mean * 1000])),
                primaryText(sprintf("Serve latency  %.1f ms",
                    [_processingStats!.serveLatency.recent.mean * 1000])),
                primaryText(sprintf("Solve attempt  %2d%%", [
                  (_processingStats!.solveAttemptFraction.recent.mean * 100)
                      .toInt()
                ])),
                primaryText(sprintf("Solve success  %d%%", [
                  (_processingStats!.solveSuccessFraction.recent.mean * 100)
                      .toInt()
                ])),
              ],
            ),
    ];
  }

  Widget mainImage() {
    return CustomPaint(
      foregroundPainter: _MainImagePainter(this),
      child: dart_widgets.Image.memory(_imageBytes, gaplessPlayback: true),
    );
  }

  Widget calibratingPacifier(BuildContext context) {
    return Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: <Widget>[
          Text("Calibrating",
              style: TextStyle(
                  fontSize: 20,
                  backgroundColor: Colors.black,
                  color: Theme.of(context).colorScheme.primary)),
          const SizedBox(height: 15),
          CircularProgressIndicator(
              value: _calibrationProgress,
              color: Theme.of(context).colorScheme.primary),
          const SizedBox(height: 15),
          TextButton(
            onPressed: () {
              cancelCalibration();
            },
            style: TextButton.styleFrom(
                backgroundColor: Colors.black,
                foregroundColor: Theme.of(context).colorScheme.primary),
            child: const Text('Cancel'),
          ),
        ]);
  }

  Widget imageStack(BuildContext context) {
    return Stack(
      alignment: Alignment.topRight,
      children: <Widget>[
        _prevFrameId != -1 ? mainImage() : Container(),
        _prevFrameId != -1 && _setupMode
            ? dart_widgets.Image.memory(_centerPeakImageBytes,
                height: _centerPeakHeight.toDouble() * 3,
                width: _centerPeakWidth.toDouble() * 3,
                fit: BoxFit.fill,
                gaplessPlayback: true)
            : Container(),
        _calibrating
            ? Positioned.fill(
                child: Align(
                    alignment: Alignment.center,
                    child: calibratingPacifier(context)))
            : Container(),
      ],
    );
  }

  Widget orientationLayout(BuildContext context) {
    if (MediaQuery.of(context).orientation == Orientation.portrait) {
      return Column(
        children: <Widget>[
          Row(children: controls()),
          const SizedBox(width: 15, height: 15),
          imageStack(context),
          const SizedBox(width: 15, height: 15),
          Row(children: dataItems(context)),
        ],
      );
    } else {
      // Landscape
      return Row(
        children: <Widget>[
          Column(children: controls()),
          const SizedBox(width: 15, height: 15),
          imageStack(context),
          const SizedBox(width: 15, height: 15),
          Column(children: dataItems(context)),
        ],
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    goFullScreen();
    // This method is rerun every time setState() is called.
    return Scaffold(
      appBar: AppBar(
          title: Text(widget.title),
          foregroundColor: Theme.of(context).colorScheme.primary),
      body: FittedBox(child: orientationLayout(context)),
      onDrawerChanged: (isOpened) {
        _doRefreshes = !isOpened;
      },
      drawer: Drawer(
          width: 200,
          child:
              ListView(padding: EdgeInsets.zero, children: drawerControls())),
    );
  }
}
