import 'dart:developer';

import 'package:cedar_flutter/main.dart';
import 'package:flutter/material.dart';
import 'package:geolocator/geolocator.dart';
import 'package:flutter_map/flutter_map.dart';
import 'package:latlong2/latlong.dart';

Future<Position?> getLocation() async {
  bool serviceEnabled = await Geolocator.isLocationServiceEnabled();
  if (!serviceEnabled) {
    log("Location services not enabled");
    return null;
  }
  LocationPermission permission = await Geolocator.checkPermission();
  if (permission == LocationPermission.denied) {
    permission = await Geolocator.requestPermission();
    if (permission == LocationPermission.denied) {
      log("Location permissions are denied");
      return null;
    }
  }
  if (permission == LocationPermission.deniedForever) {
    log("Location permissions are denied forever");
    return null;
  }
  var position = await Geolocator.getCurrentPosition(
      desiredAccuracy: LocationAccuracy.low);
  return position;
}

class MapScreen extends StatefulWidget {
  final MyHomePageState _homePageState;
  const MapScreen(this._homePageState, {Key? key}) : super(key: key);

  @override
  // ignore: library_private_types_in_public_api
  _MapScreenState createState() => _MapScreenState();
}

class _MapScreenState extends State<MapScreen> {
  final _mapController = MapController();

  @override
  Widget build(BuildContext context) {
    LatLng? selectedPosition = widget._homePageState.mapPosition;
    LatLng initialCenter = const LatLng(0, 0);
    var initialZoom = 2.0;
    if (selectedPosition != null) {
      initialCenter = selectedPosition;
      initialZoom = 5.0;
    }
    // TODO: initialCenter time zone if no selected position.
    return Scaffold(
      appBar: AppBar(title: const Text('Select Location')),
      body: Stack(children: [
        FlutterMap(
          mapController: _mapController,
          options: MapOptions(
            initialCenter: initialCenter,
            initialZoom: initialZoom,
            minZoom: 1.0,
            maxZoom: 7.0,
            interactionOptions: const InteractionOptions(
                flags: InteractiveFlag.all &
                    ~InteractiveFlag.doubleTapZoom &
                    ~InteractiveFlag.rotate),
            onTap: (tapPosition, point) {
              setState(() {
                // TODO: call a method instead, we'll want to update server.
                widget._homePageState.mapPosition = point;
              });
            },
          ),
          children: [
            TileLayer(
                urlTemplate: 'assets/tiles/{z}/{x}/{y}{r}.webp',
                tileProvider: AssetTileProvider(),
                maxNativeZoom: 6,
                retinaMode: true),
            if (selectedPosition != null)
              MarkerLayer(
                markers: [
                  Marker(
                    point: selectedPosition,
                    child: const Icon(Icons.location_pin, color: Colors.red),
                  ),
                ],
              ),
          ],
        ),
        Positioned(
          bottom: 20.0,
          right: 20.0,
          child: Column(
            children: [
              FloatingActionButton.small(
                heroTag: null,
                onPressed: () {
                  _mapController.move(_mapController.camera.center,
                      _mapController.camera.zoom + 1);
                },
                elevation: 0,
                hoverElevation: 0,
                backgroundColor: const Color(0xc0ffffff),
                foregroundColor: Theme.of(context).colorScheme.onPrimary,
                child: const Icon(Icons.add),
              ),
              const SizedBox(height: 10.0),
              FloatingActionButton.small(
                heroTag: null,
                onPressed: () {
                  _mapController.move(_mapController.camera.center,
                      _mapController.camera.zoom - 1);
                },
                elevation: 0,
                hoverElevation: 0,
                backgroundColor: const Color(0xc0ffffff),
                foregroundColor: Theme.of(context).colorScheme.onPrimary,
                child: const Icon(Icons.remove),
              ),
            ],
          ),
        ),
        const Positioned(
          bottom: 20.0,
          left: 20.0,
          child: Text(
            "© MapTiler © OpenStreetMap contributors",
            textScaleFactor: 0.5,
            style: TextStyle(color: Colors.blueGrey),
          ),
        ),
      ]),
    );
  }
}
