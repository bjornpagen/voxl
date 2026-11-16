// x264's log callback. A va_list can only be read in C, so the line is formatted here and
// handed to voxl's log (voxl_x264_line, in encoder.rs), which queues it for the log thread:
// the thread that is encoding never writes it.

#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>

void voxl_x264_line(int level, const char *line, size_t len);

void voxl_x264_log(void *private_data, int level, const char *format, va_list args)
{
    (void)private_data;
    // Longer than any line voxl's log keeps (LINE in logging.rs, 256 bytes with its
    // target), so a longer line is cut, and marked, there.
    char line[512];
    int n = vsnprintf(line, sizeof line, format, args);
    if (n < 0)
        return;
    voxl_x264_line(level, line, (size_t)n < sizeof line ? (size_t)n : sizeof line - 1);
}
