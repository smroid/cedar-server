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

void _drawBullseye(Canvas canvas, Offset boresight) {
  // Draw center bullseye.
  canvas.drawCircle(
      boresight,
      40,
      Paint()
        ..color = Colors.red
        ..strokeWidth = _hairline
        ..style = PaintingStyle.stroke);
  drawGapCross(canvas, boresight, 40, 9, _hairline);
}

void drawSlewTarget(Canvas canvas, Offset boresight, Offset? slewTarget,
    double targetDistance, double targetAngle) {
  var angleRad = _deg2rad(targetAngle + 90);

  var distanceText = "";
  if (targetDistance > 1) {
    distanceText = sprintf("%.1f°", [targetDistance]);
  } else {
    var distanceMinutes = targetDistance * 60;
    if (distanceMinutes > 1) {
      distanceText = sprintf("%.1f'", [distanceMinutes]);
    } else {
      var distanceSeconds = distanceMinutes * 60;
      distanceText = sprintf("%.1f'';", [distanceSeconds]);
    }
  }

  var arrowDistance = 60;
  var arrowStart = Offset(boresight.dx + arrowDistance * math.cos(angleRad),
      boresight.dy - arrowDistance * math.sin(angleRad));
  _drawBullseye(canvas, boresight);
  if (slewTarget == null) {
    // Slew target is not in field of view.
    drawArrow(canvas, arrowStart, 50, angleRad, distanceText, _thin);
  } else {
    var textPos = Offset(boresight.dx - 50, boresight.dy - 50);
    drawText(canvas, textPos, distanceText);
    // Draw the slew target.
    drawGapCross(canvas, slewTarget, 10, 3, _thick);
  }
}
