import 'dart:developer';

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
  // TODO: drop log
  log("position $position");
  return position;
}

class MapScreen extends StatefulWidget {
  const MapScreen({Key? key}) : super(key: key);
  @override
  _MapScreenState createState() => _MapScreenState();
}

class _MapScreenState extends State<MapScreen> {
  final _mapController = MapController();
  LatLng? _selectedPosition;
  LatLng _currentCenter = LatLng(37.45, -122.18);

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Select Location')),
      body: Stack(children: [
        FlutterMap(
          mapController: _mapController,
          options: MapOptions(
            // TODO: initialCenter according to latlng if we have it,
            // time zone otherwise.
            initialCenter: _currentCenter,
            initialZoom: 3.0,  // TODO: initial zoom more if we already have a good position
            minZoom: 1.0,
            maxZoom: 7.0,
            interactionOptions: const InteractionOptions(
                flags: InteractiveFlag.all
                    & ~InteractiveFlag.doubleTapZoom
                    & ~InteractiveFlag.rotate),
            cameraConstraint: CameraConstraint.contain(
                bounds: LatLngBounds(
                    const LatLng(80.0, 180.0), const LatLng(-80.0, -180.0))),
            onTap: (tapPosition, point) {
              setState(() {
                _selectedPosition = point;
                // TODO: remove log
                log('Tapped: ${point.latitude}, ${point.longitude}');
              });
            },
          ),
          children: [
            TileLayer(
              urlTemplate: 'assets/tiles/{z}/{x}/{y}{r}.webp',
              tileProvider: AssetTileProvider(),
              maxNativeZoom: 6,
              retinaMode: true
            ),
            if (_selectedPosition != null)
              MarkerLayer(
                markers: [
                  Marker(
                    point: _selectedPosition!,
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
                backgroundColor: const Color(0x00000000),
                foregroundColor: Theme.of(context).colorScheme.onPrimary,
                child: const Icon(Icons.zoom_in),
              ),
              const SizedBox(height: 10.0),
              FloatingActionButton.small(
                heroTag: null,
                onPressed: () {
                  _mapController.move(_mapController.camera.center,
                      _mapController.camera.zoom - 1);
                },
                backgroundColor: const Color(0x00000000),
                foregroundColor: Theme.of(context).colorScheme.onPrimary,
                child: const Icon(Icons.zoom_out),
              ),
            ],
          ),
        ),
      ]),
    );
  }
}
