import 'dart:developer';
import 'dart:math' as math;
import 'package:cedar_flutter/draw_slew_target.dart';
import 'package:cedar_flutter/draw_util.dart';
import 'package:cedar_flutter/exp_values.dart';
import 'package:cedar_flutter/geolocation.dart';
import 'package:cedar_flutter/google/protobuf/timestamp.pb.dart';
import 'package:cedar_flutter/server_log.dart';
import 'package:cedar_flutter/settings.dart';
import 'package:cedar_flutter/themes.dart';
import 'package:fixnum/fixnum.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart' as dart_widgets;
import 'package:grpc/service_api.dart';
import 'package:latlong2/latlong.dart';
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
  WidgetsFlutterBinding.ensureInitialized();
  SystemChrome.setPreferredOrientations(
    // Note that this has no effect when running as web app; it only works for
    // Android app. Not sure if it works for IOS app.
    [DeviceOrientation.landscapeLeft, DeviceOrientation.landscapeRight],
  ).then(
    (_) => runApp(MultiProvider(
      providers: [
        ChangeNotifierProvider(create: (context) => SettingsModel()),
        ChangeNotifierProvider(create: (context) => ThemeModel()),
      ],
      child: const MyApp(),
    )),
  );
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
  State<MyHomePage> createState() => MyHomePageState();
}

double _durationToMs(proto_duration.Duration duration) {
  return duration.seconds.toDouble() * 1000 +
      (duration.nanos.toDouble()) / 1000000;
}

proto_duration.Duration _msToDuration(int ms) {
  var duration = proto_duration.Duration();
  duration.seconds = Int64(ms ~/ 1000);
  duration.nanos = (ms * 1000000) % 1000000000;
  return duration;
}

double _deg2rad(double deg) {
  return deg / 180.0 * math.pi;
}

class _MainImagePainter extends CustomPainter {
  final MyHomePageState state;
  final BuildContext _context;

  _MainImagePainter(this.state, this._context);

  @override
  void paint(Canvas canvas, Size size) {
    const double hairline = 0.5;
    const double thin = 1;
    final Color color = Theme.of(_context).colorScheme.primary;
    if (state._setupMode && state._centerRegion != null) {
      // Draw search box within which we search for the brightest star for
      // focusing.
      canvas.drawRect(
          state._centerRegion as Rect,
          Paint()
            ..color = color
            ..strokeWidth = thin
            ..style = PaintingStyle.stroke);
      // Draw box around location of the brightest star in search box.
      canvas.drawRect(
          state._centerPeakRegion as Rect,
          Paint()
            ..color = color
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
              ..color = color
              ..strokeWidth = hairline
              ..style = PaintingStyle.stroke);
      }
    }

    // How many display pixels is the telescope FOV?
    var scopeFov = 0.0;
    if (!state._setupMode && state._hasSolution) {
      scopeFov = state._preferences!.eyepieceFov *
          state._imageRegion.width /
          state._solutionFOV;
    }
    if (state._slewRequest != null && !state._setupMode && state._hasSolution) {
      var slew = state._slewRequest;
      Offset? posInImage;
      if (slew!.hasImagePos()) {
        posInImage = Offset(slew.imagePos.x / state._binFactor,
            slew.imagePos.y / state._binFactor);
      }
      drawSlewTarget(
          canvas,
          color,
          state._boresightPosition,
          scopeFov,
          /*rollAngleRad=*/ _deg2rad(state.bullseyeDirectionIndicator()),
          posInImage,
          slew.targetDistance,
          slew.targetAngle);
      drawSlewDirections(
          canvas,
          color,
          slew.targetAngle >= 0.0 &&
                  slew.targetAngle <= 90.0 &&
                  slew.targetDistance > 0.5
              ? Offset(20, state._imageRegion.height - 220)
              : const Offset(20, 20),
          state._preferences?.mountType == MountType.ALT_AZ,
          state._northernHemisphere,
          slew.offsetRotationAxis,
          slew.offsetTiltAxis);
    } else {
      // Make a cross at the boresight position (if any) or else the image
      // center.
      if (state._setupMode || !state._hasSolution) {
        drawCross(canvas, color, state._boresightPosition, /*radius=*/ 8,
            /*rollAngleRad=*/ 0.0, thin, thin);
      } else {
        var rollAngleRad = _deg2rad(state.bullseyeDirectionIndicator());
        drawBullseye(canvas, color, state._boresightPosition, scopeFov / 2,
            rollAngleRad);
      }
    }
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) {
    return true;
  }
}

class _OverlayImagePainter extends CustomPainter {
  final MyHomePageState _state;
  final BuildContext _context;
  final double _scale;

  _OverlayImagePainter(this._state, this._context, this._scale);

  @override
  void paint(Canvas canvas, Size size) {
    final Color color = Theme.of(_context).colorScheme.primary;
    Offset overlayCenter = Offset(size.width / 2, size.height / 2);

    var slew = _state._slewRequest;
    Offset? posInImage;
    if (slew!.hasImagePos()) {
      posInImage = Offset(
          overlayCenter.dx +
              _scale * (slew.imagePos.x - _state._fullResBoresightPosition.dx),
          overlayCenter.dy +
              _scale * (slew.imagePos.y - _state._fullResBoresightPosition.dy));
    }
    // How many display pixels is the telescope FOV?
    final scopeFov = _scale *
        _state._preferences!.eyepieceFov *
        _state._fullResImageRegion.width /
        _state._solutionFOV;
    drawSlewTarget(
        canvas,
        color,
        overlayCenter,
        scopeFov,
        /*rollAngleRad=*/ _deg2rad(_state.bullseyeDirectionIndicator()),
        posInImage,
        slew.targetDistance,
        slew.targetAngle,
        drawDistanceText: false);
  }

  @override
  bool shouldRepaint(covariant CustomPainter oldDelegate) {
    return true;
  }
}

class MyHomePageState extends State<MyHomePage> {
  MyHomePageState() {
    refreshStateFromServer();
  }

  // Geolocation from map.
  LatLng? _mapPosition;
  bool _northernHemisphere = true;

  Duration _tzOffset = const Duration();

  // Information from most recent FrameResult.

  // Image data, binned by server.
  Uint8List _imageBytes = Uint8List(1);
  late Rect _imageRegion; // Scaled by _binFactor.
  late Rect _fullResImageRegion;
  int _binFactor = 1;

  OperationSettings? _operationSettings;
  bool _setupMode = false;
  bool _canAlign = false;

  int _accuracy = 2; // 1-3.

  Offset _boresightPosition =
      const Offset(0, 0); // Scaled by main image's binning.
  Offset _fullResBoresightPosition = const Offset(0, 0);

  Rect? _centerRegion; // Scaled by main image's binning.
  Rect? _centerPeakRegion; // Scaled by binning.

  int _centerPeakWidth = 0;
  int _centerPeakHeight = 0;
  Uint8List? _centerPeakImageBytes;

  int _boresightImageWidth = 0;
  int _boresightImageHeight = 0;
  Uint8List? _boresightImageBytes;

  int _prevFrameId = -1;
  late List<StarCentroid> _stars;
  int _numStars = 0;
  double _exposureTimeMs = 0.0;
  int _maxExposureTimeMs = 0;
  bool _hasSolution = false;

  List<MatchedStar>? _solutionMatches;
  List<Offset>? _solutionCentroids;
  // Degrees.
  double _solutionRA = 0.0;
  double _solutionDec = 0.0;
  double _solutionRollAngle = 0.0; // Degrees.
  double _solutionFOV = 0.0; // Degrees.

  // Arcsec.
  double _solutionRMSE = 0.0;

  LocationBasedInfo? _locationBasedInfo;

  CalibrationData? _calibrationData;
  ProcessingStats? _processingStats;
  SlewRequest? _slewRequest;
  Preferences? _preferences;
  PolarAlignAdvice? _polarAlignAdvice;

  // Calibration happens when _setupMode transitions to false.
  bool _calibrating = false;
  double _calibrationProgress = 0.0;

  // Transition from Operate mode back to Setup mode can take a second or
  // so if the update rate setting is e.g.1 Hz. We put up a pacifier for this;
  // define a flag so we can know when it is done.
  bool _transitionToSetup = false;

  // Values set from on-screen controls.
  bool _doRefreshes = true;
  int _expSettingMs = 0; // 0 is auto-exposure.

  CedarClient? _client;
  CedarClient client() {
    _client ??= getClient(); // Initialize if null.
    return _client!;
  }

  LatLng? get mapPosition => _mapPosition;
  set mapPosition(LatLng? newPos) {
    setState(() {
      _mapPosition = newPos;
      if (newPos != null) {
        setObserverLocation(newPos);
      }
    });
  }

  Duration get tzOffset => _tzOffset;

  void _setStateFromOpSettings(OperationSettings opSettings) {
    _operationSettings = opSettings;
    _accuracy = opSettings.accuracy.value;
    _expSettingMs = _durationToMs(opSettings.exposureTime).toInt();
    _setupMode = opSettings.operatingMode == OperatingMode.SETUP;
    if (_setupMode) {
      _transitionToSetup = false;
    }
  }

  void setStateFromFrameResult(FrameResult response) {
    _prevFrameId = response.frameId;
    _stars = response.starCandidates;
    _numStars = _stars.length;
    _maxExposureTimeMs =
        _durationToMs(response.fixedSettings.maxExposureTime).toInt();
    if (response.fixedSettings.hasObserverLocation()) {
      _mapPosition = LatLng(response.fixedSettings.observerLocation.latitude,
          response.fixedSettings.observerLocation.longitude);
      _northernHemisphere = _mapPosition!.latitude > 0.0;
    } else if (_mapPosition != null) {
      setObserverLocation(_mapPosition!);
    }
    _hasSolution = false;
    _calibrating = response.calibrating;
    if (response.calibrating) {
      _calibrationProgress = response.calibrationProgress;
    }
    if (response.preferences.nightVisionTheme) {
      Provider.of<ThemeModel>(context, listen: false).setNightVisionTheme();
    } else {
      Provider.of<ThemeModel>(context, listen: false).setNormalTheme();
    }
    _setStateFromOpSettings(response.operationSettings);
    _canAlign = false;
    if (_setupMode) {
      _canAlign = true;
    }
    _preferences = response.preferences;
    _polarAlignAdvice = response.polarAlignAdvice;
    var settingsModel = Provider.of<SettingsModel>(context, listen: false);
    settingsModel.preferencesProto = _preferences!.deepCopy();
    settingsModel.opSettingsProto = _operationSettings!.deepCopy();
    _calibrationData =
        response.hasCalibrationData() ? response.calibrationData : null;
    _processingStats =
        response.hasProcessingStats() ? response.processingStats : null;
    _slewRequest = response.hasSlewRequest() ? response.slewRequest : null;
    if (_slewRequest != null && _slewRequest!.targetWithinCenterRegion) {
      _canAlign = true;
    }
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
        _solutionRollAngle = plateSolution.roll;
        _solutionRMSE = plateSolution.rmse;
        _solutionFOV = plateSolution.fov;
        if (response.hasLocationBasedInfo()) {
          _locationBasedInfo = response.locationBasedInfo;
        }
      }
    }
    if (response.hasImage()) {
      _imageBytes = Uint8List.fromList(response.image.imageData);
      _binFactor = response.image.binningFactor;
      _imageRegion = Rect.fromLTWH(
          0,
          0,
          response.image.rectangle.width.toDouble() / _binFactor,
          response.image.rectangle.height.toDouble() / _binFactor);
      _fullResImageRegion = Rect.fromLTWH(
          0,
          0,
          response.image.rectangle.width.toDouble(),
          response.image.rectangle.height.toDouble());
    }
    _boresightPosition = Offset(response.boresightPosition.x / _binFactor,
        response.boresightPosition.y / _binFactor);
    _fullResBoresightPosition =
        Offset(response.boresightPosition.x, response.boresightPosition.y);
    if (response.hasCenterRegion()) {
      var cr = response.centerRegion;
      _centerRegion = Rect.fromLTWH(
          cr.originX.toDouble() / _binFactor,
          cr.originY.toDouble() / _binFactor,
          cr.width.toDouble() / _binFactor,
          cr.height.toDouble() / _binFactor);
    }
    if (response.hasExposureTime()) {
      _exposureTimeMs = _durationToMs(response.exposureTime);
    }
    _centerPeakImageBytes = null;
    if (response.hasCenterPeakImage()) {
      _centerPeakImageBytes =
          Uint8List.fromList(response.centerPeakImage.imageData);
      _centerPeakWidth = response.centerPeakImage.rectangle.width;
      _centerPeakHeight = response.centerPeakImage.rectangle.height;
    }
    _boresightImageBytes = null;
    if (response.hasBoresightImage()) {
      _boresightImageBytes =
          Uint8List.fromList(response.boresightImage.imageData);
      _boresightImageWidth = response.boresightImage.rectangle.width;
      _boresightImageHeight = response.boresightImage.rectangle.height;
    }
    if (response.hasCenterPeakPosition()) {
      var cp = response.centerPeakPosition;
      _centerPeakRegion = Rect.fromCenter(
          center: Offset(cp.x / _binFactor, cp.y / _binFactor),
          width: _centerPeakWidth.toDouble() / _binFactor,
          height: _centerPeakHeight.toDouble() / _binFactor);
    }
  }

  double bullseyeDirectionIndicator() {
    if (_preferences?.mountType == MountType.ALT_AZ &&
        _locationBasedInfo != null) {
      return _locationBasedInfo!.zenithRollAngle;
    } else {
      return _solutionRollAngle; // Direction towards north.
    }
  }

  Future<void> updateFixedSettings(FixedSettings request) async {
    try {
      await client().updateFixedSettings(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
    } catch (e) {
      log('updateFixedSettings error: $e');
    }
  }

  Future<void> updateOperationSettings(OperationSettings request) async {
    try {
      var newOpSettings = await client().updateOperationSettings(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
      setState(() {
        _setStateFromOpSettings(newOpSettings);
      });
    } catch (e) {
      log('updateOperationSettings error: $e');
    }
  }

  // Use request/response style of RPC.
  Future<void> getFrameFromServer() async {
    final request = FrameRequest()..prevFrameId = _prevFrameId;
    try {
      final response = await client().getFrame(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
      setState(() {
        setStateFromFrameResult(response);
      });
    } catch (e) {
      log('getFrameFromServer error: $e');
    }
  }

  // Issue repeated request/response RPCs.
  Future<void> refreshStateFromServer() async {
    // See if we can get location from the platform. If we are a web app, served
    // over http (not https), we won't be able to get location here.
    var platformPosition = await getLocation();
    if (platformPosition != null) {
      _mapPosition =
          LatLng(platformPosition.latitude, platformPosition.longitude);
    }

    // Get platform time.
    final now = DateTime.now();
    _tzOffset = now.timeZoneOffset;
    setServerTime(now);

    await Future.doWhile(() async {
      var delay = 100;
      if (_setupMode && !_calibrating && _doRefreshes) {
        delay = 10; // Fast updates for focusing.
      }
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
      log('initiateAction error: $e');
    }
  }

  Future<void> setServerTime(DateTime now) async {
    Timestamp ts = Timestamp();
    ts.seconds = Int64(now.millisecondsSinceEpoch ~/ 1000.0);
    ts.nanos = (now.millisecondsSinceEpoch % 1000) * 1000000;
    var request = FixedSettings();
    request.currentTime = ts;
    await updateFixedSettings(request);
  }

  Future<void> setObserverLocation(LatLng pos) async {
    LatLong posProto = LatLong();
    posProto.latitude = pos.latitude;
    posProto.longitude = pos.longitude;
    var request = FixedSettings();
    request.observerLocation = posProto;
    await updateFixedSettings(request);
  }

  Future<void> setExpTime() async {
    var request = OperationSettings();
    request.exposureTime = _msToDuration(_expSettingMs);
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

  Future<String> getServerLogs() async {
    var request = ServerInformationRequest();
    request.logRequest = 20000;
    try {
      var infoResult = await client().getServerInformation(request,
          options: CallOptions(timeout: const Duration(seconds: 10)));
      return infoResult.logContent;
    } catch (e) {
      log('getServerLogs error: $e');
      return "";
    }
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

  Future<void> updatePreferences(Preferences changedPrefs) async {
    try {
      final newPrefs = await client().updatePreferences(changedPrefs,
          options: CallOptions(timeout: const Duration(seconds: 10)));
      setState(() {
        _preferences = newPrefs;
        if (newPrefs.nightVisionTheme) {
          Provider.of<ThemeModel>(context, listen: false).setNightVisionTheme();
        } else {
          Provider.of<ThemeModel>(context, listen: false).setNormalTheme();
        }
      });
    } catch (e) {
      log('updatePreferences error: $e');
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
            max: 3,
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
          primaryText("Exposure time"),
          NumberPicker(
            axis: Axis.horizontal,
            itemWidth: 50,
            itemHeight: 40,
            minValue: 0,
            maxValue: expMsToIndex(_maxExposureTimeMs),
            value: _expSettingMs == 0 ? 0 : expMsToIndex(_expSettingMs),
            onChanged: (value) => {
              setState(() {
                _expSettingMs = value == 0 ? 0 : expMsFromIndex(value);
                setExpTime();
              })
            },
            textMapper: (numberText) {
              int expIndex = int.parse(numberText);
              if (expIndex == 0) {
                return "auto";
              }
              int expMs = expMsFromIndex(expIndex);
              if (expMs < 1000) {
                return sprintf("%d", [expMs]);
              } else {
                return sprintf("%.1fs", [expMs / 1000]);
              }
            },
            // selectedTextStyle: TextStyle(fontSize: 20),
          ),
          _expSettingMs == 0
              ? primaryText(sprintf("auto %.1f ms", [_exposureTimeMs]))
              : primaryText(sprintf("%d ms", [_expSettingMs])),
          const SizedBox(height: 15),
        ],
      ),
      TextButton.icon(
          label: const Text("Preferences"),
          icon: const Icon(Icons.settings),
          onPressed: () {
            Navigator.push(
                context,
                MaterialPageRoute(
                    builder: (context) => const SettingsScreen()));
          }),
      const SizedBox(height: 15),
      TextButton.icon(
          label: _mapPosition == null
              ? const Text("Location unknown")
              : Text(sprintf("Location %.1f %.1f",
                  [_mapPosition!.latitude, _mapPosition!.longitude])),
          icon: Icon(_mapPosition == null
              ? Icons.not_listed_location
              : Icons.edit_location_alt),
          onPressed: () {
            Navigator.push(context,
                MaterialPageRoute(builder: (context) => MapScreen(this)));
          }),
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
            child: const Text("Show server log"),
            onPressed: () async {
              var logs = await getServerLogs();
              // ignore: use_build_context_synchronously
              showDialog(
                  context: context, builder: (context) => ServerLogPopUp(logs));
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
          }
          final newOpSettings = settings.opSettingsProto;
          var opSettingsDiff = newOpSettings.deepCopy();
          if (_operationSettings != null &&
              diffOperationSettings(_operationSettings!, opSettingsDiff)) {
            updateOperationSettings(opSettingsDiff);
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
                  if (!value) {
                    _transitionToSetup = true;
                  }
                  setOperatingMode(/*setup=*/ !value);
                });
              }),
          primaryText("Aim"),
        ])
      ]),
      const SizedBox(width: 15, height: 15),
      SizedBox(
        width: 120,
        height: 32,
        child: _canAlign
            ? OutlinedButton(
                child: const Text("Set Align"),
                onPressed: () {
                  captureBoresight();
                })
            : Container(),
      ),
      const SizedBox(width: 15, height: 15),
      SizedBox(
        width: 120,
        height: 32,
        child: _slewRequest != null && !_setupMode
            ? OutlinedButton(
                child: const Text("End goto"),
                onPressed: () {
                  stopSlew();
                })
            : Container(),
      ),
    ];
  }

  String formatRightAscension(double ra) {
    if (_preferences?.celestialCoordFormat == CelestialCoordFormat.DECIMAL) {
      return sprintf("RA %.4f°", [ra]);
    }
    int hours = (ra / 15.0).floor();
    double fracHours = ra / 15.0 - hours;
    int minutes = (fracHours * 60.0).floor();
    double fracMinutes = fracHours * 60.0 - minutes;
    int seconds = (fracMinutes * 60).round();
    return sprintf("RA %02dh %02dm %02ds", [hours, minutes, seconds]);
  }

  String formatHourAngle(double ha) {
    if (_preferences?.celestialCoordFormat == CelestialCoordFormat.DECIMAL) {
      return sprintf("HA %.4f°", [ha]);
    }
    String sign = ha < 0 ? "-" : "+";
    if (ha < 0) {
      ha = -ha;
    }
    int hours = (ha / 15.0).floor();
    double fracHours = ha / 15.0 - hours;
    int minutes = (fracHours * 60.0).floor();
    double fracMinutes = fracHours * 60.0 - minutes;
    int seconds = (fracMinutes * 60).round();
    return sprintf("HA %s%02dh %02dm %02ds", [sign, hours, minutes, seconds]);
  }

  String formatDeclination(double dec) {
    if (_preferences?.celestialCoordFormat == CelestialCoordFormat.DECIMAL) {
      return sprintf("Dec %.4f°", [dec]);
    }
    String sign = dec < 0 ? "-" : "+";
    if (dec < 0) {
      dec = -dec;
    }
    int degrees = dec.floor();
    double fracDegrees = dec - degrees;
    int minutes = (fracDegrees * 60.0).floor();
    double fracMinutes = fracDegrees * 60.0 - minutes;
    int seconds = (fracMinutes * 60).round();
    return sprintf(
        "Dec %s%02d° %02d' %02d''", [sign, degrees, minutes, seconds]);
  }

  String formatAltitude(double alt) {
    return sprintf("Alt %.3f°", [alt]);
  }

  String formatAzimuth(double az) {
    final String dir = switch (az) {
      >= 360 - 22.5 || < 22.5 => "N",
      >= 22.5 && < 45 + 22.5 => "NE",
      >= 45 + 22.5 && < 90 + 22.5 => "E",
      >= 90 + 22.5 && < 135 + 22.5 => "SE",
      >= 135 + 22.5 && < 180 + 22.5 => "S",
      >= 180 + 22.5 && < 225 + 22.5 => "SW",
      >= 225 + 22.5 && < 270 + 22.5 => "W",
      >= 270 + 22.5 && < 315 + 22.5 => "NW",
      double() => "??",
    };
    return sprintf("Az %.3f° %s", [az, dir]);
  }

  String formatAdvice(ErrorBoundedValue? ebv) {
    return sprintf("%.2f±%.2f", [ebv!.value, ebv.error]);
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

  bool hasPolarAdvice() {
    if (_polarAlignAdvice == null) {
      return false;
    }
    return _polarAlignAdvice!.hasAltitudeCorrection() ||
        _polarAlignAdvice!.hasAzimuthCorrection();
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
              width: 120,
              height: 120,
              child: Column(
                children: <Widget>[
                  primaryText("Aim"),
                  solveText(sprintf("%s", [formatRightAscension(_solutionRA)])),
                  solveText(sprintf("%s", [formatDeclination(_solutionDec)])),
                  solveText(sprintf("RMSE %.1f", [_solutionRMSE])),
                  if (_locationBasedInfo != null)
                    solveText(sprintf("%s",
                        [formatHourAngle(_locationBasedInfo!.hourAngle)])),
                  if (_locationBasedInfo != null)
                    solveText(sprintf(
                        "%s", [formatAltitude(_locationBasedInfo!.altitude)])),
                  if (_locationBasedInfo != null)
                    solveText(sprintf(
                        "%s", [formatAzimuth(_locationBasedInfo!.azimuth)])),
                ],
              )),
      const SizedBox(width: 15, height: 15),
      _slewRequest == null || _setupMode
          ? Container()
          : SizedBox(
              width: 120,
              height: 80,
              child: Column(children: <Widget>[
                primaryText("Target"),
                solveText(sprintf(
                    "%s", [formatRightAscension(_slewRequest!.target.ra)])),
                solveText(sprintf(
                    "%s", [formatDeclination(_slewRequest!.target.dec)])),
                solveText(
                    sprintf("%.1f° away", [_slewRequest?.targetDistance])),
              ]),
            ),
      const SizedBox(width: 15, height: 15),
      !hasPolarAdvice() || _setupMode
          ? Container()
          : SizedBox(
              width: 140,
              height: 80,
              child: Column(children: <Widget>[
                primaryText("Polar Align"),
                _polarAlignAdvice!.hasAltitudeCorrection()
                    ? solveText(sprintf("alt %s", [
                        sprintf("%s\npolar axis->%s", [
                          formatAdvice(_polarAlignAdvice!.altitudeCorrection),
                          _polarAlignAdvice!.altitudeCorrection.value > 0
                              ? "up"
                              : "down"
                        ])
                      ]))
                    : Container(),
                _polarAlignAdvice!.hasAzimuthCorrection()
                    ? solveText(sprintf("az %s", [
                        sprintf("%s\npolar axis->%s", [
                          formatAdvice(_polarAlignAdvice!.azimuthCorrection),
                          _polarAlignAdvice!.azimuthCorrection.value > 0
                              ? "right"
                              : "left"
                        ])
                      ]))
                    : Container(),
              ]),
            ),
      const SizedBox(width: 15, height: 15),
      _setupMode || _processingStats == null || !_preferences!.showPerfStats
          ? Container()
          : Column(
              children: <Widget>[
                primaryText(sprintf("Detect %.1f ms",
                    [_processingStats!.detectLatency.recent.mean * 1000])),
                primaryText(sprintf("Solve %.1f ms",
                    [_processingStats!.solveLatency.recent.mean * 1000])),
                primaryText(sprintf("Solve attempt %2d%%", [
                  (_processingStats!.solveAttemptFraction.recent.mean * 100)
                      .toInt()
                ])),
                primaryText(sprintf("Solve success %d%%", [
                  (_processingStats!.solveSuccessFraction.recent.mean * 100)
                      .toInt()
                ])),
              ],
            ),
    ];
  }

  Widget mainImage() {
    return ClipRect(
        child: CustomPaint(
      foregroundPainter: _MainImagePainter(this, context),
      child: dart_widgets.Image.memory(_imageBytes, gaplessPlayback: true),
    ));
  }

  Widget pacifier(BuildContext context, bool calibrating) {
    return Column(
        mainAxisAlignment: MainAxisAlignment.center,
        children: <Widget>[
          calibrating
              ? Text("Calibrating",
                  style: TextStyle(
                      fontSize: 20,
                      backgroundColor: Colors.black,
                      color: Theme.of(context).colorScheme.primary))
              : Container(),
          const SizedBox(height: 15),
          CircularProgressIndicator(
              value: calibrating ? _calibrationProgress : null,
              color: Theme.of(context).colorScheme.primary),
          const SizedBox(height: 15),
          calibrating
              ? TextButton(
                  onPressed: () {
                    cancelCalibration();
                  },
                  style: TextButton.styleFrom(
                      backgroundColor: Colors.black,
                      foregroundColor: Theme.of(context).colorScheme.primary),
                  child: const Text('Cancel'),
                )
              : Container(),
        ]);
  }

  Widget imageStack(BuildContext context) {
    Widget? overlayWidget;
    if (_setupMode && _centerPeakImageBytes != null) {
      overlayWidget = dart_widgets.Image.memory(_centerPeakImageBytes!,
          height: _imageRegion.width / 6,
          width: _imageRegion.width / 6,
          fit: BoxFit.fill,
          gaplessPlayback: true);
    } else if (!_setupMode && _boresightImageBytes != null) {
      var overlayImage = dart_widgets.Image.memory(_boresightImageBytes!,
          height: _imageRegion.width / 4,
          width: _imageRegion.width / 4,
          fit: BoxFit.fill,
          gaplessPlayback: true);
      overlayWidget = ClipRect(
          child: CustomPaint(
              foregroundPainter: _OverlayImagePainter(this, context,
                  (_imageRegion.width / 4) / _boresightImageWidth),
              child: overlayImage));
    }
    return Stack(
      alignment: Alignment.topRight,
      children: <Widget>[
        _prevFrameId != -1 ? mainImage() : Container(),
        _prevFrameId != -1 && overlayWidget != null
            ? Container(
                decoration: BoxDecoration(
                    border: Border.all(
                        width: 0.5,
                        color: Theme.of(context).colorScheme.primary)),
                child: overlayWidget)
            : Container(),
        _calibrating || _transitionToSetup
            ? Positioned.fill(
                child: Align(
                    alignment: Alignment.center,
                    child: pacifier(context, _calibrating)))
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

  final _scaffoldKey = GlobalKey<ScaffoldState>();

  @override
  Widget build(BuildContext context) {
    bool hideAppBar = Provider.of<SettingsModel>(context, listen: false)
        .preferencesProto
        .hideAppBar;
    if (hideAppBar) {
      goFullScreen();
    } else {
      cancelFullScreen();
    }

    // This method is rerun every time setState() is called.
    return Scaffold(
      key: _scaffoldKey,
      appBar: AppBar(
          toolbarHeight: hideAppBar ? 0 : 56,
          toolbarOpacity: hideAppBar ? 0.0 : 1.0,
          title: Text(widget.title),
          foregroundColor: Theme.of(context).colorScheme.primary),
      body: Stack(children: [
        Positioned(
            left: 8,
            top: 0,
            child: hideAppBar
                ? IconButton(
                    icon: const Icon(Icons.menu),
                    onPressed: () {
                      _scaffoldKey.currentState!.openDrawer();
                    })
                : Container()),
        FittedBox(child: orientationLayout(context)),
      ]),
      onDrawerChanged: (isOpened) {
        _doRefreshes = !isOpened;
      },
      drawer: Drawer(
          width: 200,
          child:
              ListView(padding: EdgeInsets.zero, children: drawerControls())),
      drawerEdgeDragWidth: 100,
    );
  }
}
