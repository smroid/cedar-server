import 'dart:math' as math;
import 'package:cedar_flutter/draw_util.dart';
import 'package:flutter/material.dart';
import 'package:sprintf/sprintf.dart';

const double _hairline = 0.5;
const double _thin = 1;
const double _thick = 2;

double _deg2rad(double deg) {
  return deg / 180.0 * math.pi;
}

void _drawBullseye(Canvas canvas, Offset boresight, double radius) {
  // Draw center bullseye.
  canvas.drawCircle(
      boresight,
      radius,
      Paint()
        ..color = Colors.red
        ..strokeWidth = _thin
        ..style = PaintingStyle.stroke);
  drawGapCross(canvas, boresight, radius, 9, _hairline);
}

void drawSlewTarget(
    Canvas canvas,
    Offset boresight,
    double boresightDiameterPix,
    Offset? slewTarget,
    double targetDistance,
    double targetAngle) {
  var distanceText = "";
  if (targetDistance > 1) {
    distanceText = sprintf("%.1f°", [targetDistance]);
  } else {
    final distanceMinutes = targetDistance * 60;
    if (distanceMinutes > 1) {
      distanceText = sprintf("%.1f'", [distanceMinutes]);
    } else {
      final distanceSeconds = distanceMinutes * 60;
      distanceText = sprintf("%.1f'';", [distanceSeconds]);
    }
  }
  if (slewTarget == null) {
    // Slew target is not in field of view. Draw an arrow pointing to it.
    // Make arrow length proportional to targetDistance (degrees, up to 180).
    final arrowLength =
        math.min(200, 200 * math.sqrt(targetDistance / 180.0)).toDouble();
    final arrowRoot = -arrowLength / 2.0;
    final angleRad = _deg2rad(targetAngle + 90);
    final arrowStart = Offset(boresight.dx + arrowRoot * math.cos(angleRad),
        boresight.dy - arrowRoot * math.sin(angleRad));
    drawArrow(canvas, arrowStart, arrowLength, angleRad, distanceText, _thin);
  } else {
    // Slew target is in the field of view.
    // Draw the slew target.
    drawGapCross(canvas, slewTarget, 10, 3, _thick);
    // Draw a bullseye at the boresight position, annotated with the target
    // distance.
    final bsRadius = boresightDiameterPix / 2;
    _drawBullseye(canvas, boresight, bsRadius);
    final textPos =
        Offset(boresight.dx - bsRadius - 10, boresight.dy - bsRadius - 10);
    drawText(canvas, textPos, distanceText);
  }
}
