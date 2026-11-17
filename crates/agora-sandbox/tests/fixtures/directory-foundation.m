#import <Foundation/Foundation.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    @autoreleasepool {
        NSString *path = [NSString stringWithUTF8String:argv[1]];
        NSString *upper = [path stringByAppendingPathComponent:@"upper.txt"];
        int fd = open([upper fileSystemRepresentation], O_CREAT | O_WRONLY | O_TRUNC, 0600);
        if (fd < 0 || write(fd, "sandbox", 7) != 7 || close(fd)) return 3;
        NSString *removed = [path stringByAppendingPathComponent:@"removed.txt"];
        if (unlink([removed fileSystemRepresentation])) return 4;
        NSFileManager *manager = [NSFileManager defaultManager];
        NSError *error = nil;
        NSArray *names = [manager contentsOfDirectoryAtPath:path error:&error];
        NSArray *expected = @[@".metadata", @"lower.txt", @"upper.txt"];
        if (![[names sortedArrayUsingSelector:@selector(compare:)] isEqual:expected]) {
            NSLog(@"Unexpected names: %@, error: %@", names, error);
            return 5;
        }
        NSMutableArray *enumerated = [NSMutableArray array];
        NSDirectoryEnumerator *entries = [manager enumeratorAtURL:[NSURL fileURLWithPath:path]
            includingPropertiesForKeys:@[NSURLNameKey, NSURLIsRegularFileKey, NSURLFileSizeKey]
            options:NSDirectoryEnumerationSkipsSubdirectoryDescendants errorHandler:nil];
        for (NSURL *entry in entries) {
            NSNumber *regular = nil;
            if (![entry getResourceValue:&regular forKey:NSURLIsRegularFileKey error:&error] || !regular.boolValue) return 6;
            NSNumber *size = nil;
            NSUInteger expectedSize = [entry.lastPathComponent isEqual:@"upper.txt"] ? 7 : 4;
            if (![entry getResourceValue:&size forKey:NSURLFileSizeKey error:&error] || size.unsignedIntegerValue != expectedSize) return 8;
            [enumerated addObject:entry.lastPathComponent];
        }
        if (![[enumerated sortedArrayUsingSelector:@selector(compare:)] isEqual:expected]) {
            NSLog(@"Unexpected enumeration: %@", enumerated);
            return 7;
        }
    }
    return 0;
}
