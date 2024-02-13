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

void _drawArrow(
    Canvas canvas, Offset start, double length, double angleRad, String text) {
  var end = Offset(start.dx + length * math.cos(angleRad),
      start.dy - length * math.sin(angleRad));

  // Adapted from https://stackoverflow.com/questions/72714333
  // (flutter-how-do-i-make-arrow-lines-with-canvas).
  final paint = Paint()
    ..color = Colors.red
    ..strokeWidth = _thin;
  canvas.drawLine(start, end, paint);
  const arrowSize = 12;
  const arrowAngle = 25 * math.pi / 180;

  final path = Path();
  path.moveTo(end.dx - arrowSize * math.cos(angleRad - arrowAngle),
      end.dy + arrowSize * math.sin(angleRad - arrowAngle));
  path.lineTo(end.dx, end.dy);
  path.lineTo(end.dx - arrowSize * math.cos(angleRad + arrowAngle),
      end.dy + arrowSize * math.sin(angleRad + arrowAngle));
  path.close();
  canvas.drawPath(path, paint);

  var textPos = Offset(start.dx + (length + 20) * math.cos(angleRad) - 10,
      start.dy - (length + 20) * math.sin(angleRad) - 10);
  _drawText(canvas, textPos, text);
}

void _drawText(Canvas canvas, Offset pos, String text) {
  final textPainter = TextPainter(
      text: TextSpan(
          text: text, style: const TextStyle(color: Colors.red, fontSize: 14)),
      textDirection: TextDirection.ltr,
      textAlign: TextAlign.center);
  textPainter.layout();
  textPainter.paint(canvas, pos);
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
    _drawArrow(canvas, arrowStart, 50, angleRad, distanceText);
  } else {
    var textPos = Offset(boresight.dx - 50, boresight.dy - 50);
    _drawText(canvas, textPos, distanceText);
    // Draw the slew target.
    drawGapCross(canvas, slewTarget, 10, 3, _thick);
  }
}
