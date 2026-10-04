---@meta

---@class iolib
io = {}

---Standard input stream.
---@type file
io.stdin = {}

---Standard output stream.
---@type file
io.stdout = {}

---Standard error stream.
---@type file
io.stderr = {}

---Closes `f`; see `file:close`. FiveM has no default output file to close when `f` is omitted.
---@param f file
---@return boolean? ok
---@return string? exitcode
---@return integer? code
function io.close(f) end

---Does nothing: FiveM has no default output file.
function io.flush() end

---Returns an iterator that yields nothing: FiveM does not read files here. Open the file with `io.open` and use `file:lines` instead.
---@param filename? string
---@param ... read_format
---@return fun(): any, ...any iterator
function io.lines(filename, ...) end

---Opens a file in the given mode (default "r"). Returns the handle, or nil plus an error message and code.
---@param filename string
---@param mode? open_mode
---@return file? handle
---@return string? err
---@return integer? code
---@nodiscard
function io.open(filename, mode) end

---Starts `prog` in a separate process and returns a handle to read from ("r", default) or write to ("w") it. Not available on every platform.
---@param prog string
---@param mode? popen_mode
---@return file? handle
---@return string? err
function io.popen(prog, mode) end

---Returns a handle to a temporary file opened in update mode that is removed when the program ends.
---@return file handle
---@nodiscard
function io.tmpfile() end

---Returns "file" for an open handle, "closed file" for a closed one and nil for anything else.
---@param obj any
---@return "file"|"closed file"|nil kind
---@nodiscard
function io.type(obj) end

---Prints its arguments to the server console like `print`: FiveM has no default output file.
---@param ... string|number
function io.write(...) end

---@class oslib
os = {}

---Returns an approximation of the CPU time used by the program, in seconds.
---@return number seconds
---@nodiscard
function os.clock() end

---Formats a time (default: now) using strftime-style `format`. A leading "!" selects UTC and the format "*t" returns a table of date fields.
---@param format? string
---@param time? integer
---@return string|osdate result
---@nodiscard
function os.date(format, time) end

---Returns the number of seconds from time `t1` to time `t2`.
---@param t2 integer
---@param t1 integer
---@return number seconds
---@nodiscard
function os.difftime(t2, t1) end

---Runs `command` through the system shell and returns success, "exit" or "signal", and the status code. Without a command it reports whether a shell is available.
---@param command? string
---@return boolean? ok
---@return string? exitcode
---@return integer? code
function os.execute(command) end

---Returns the value of an environment variable, or nil when it is not defined.
---@param varname string
---@return string? value
---@nodiscard
function os.getenv(varname) end

---Deletes a file or an empty directory. Returns true, or nil plus an error message and code.
---@param filename string
---@return boolean? ok
---@return string? err
---@return integer? code
function os.remove(filename) end

---Renames or moves a file or directory. Returns true, or nil plus an error message and code.
---@param oldname string
---@param newname string
---@return boolean? ok
---@return string? err
---@return integer? code
function os.rename(oldname, newname) end

---Sets the program locale for the given category, or queries it when `locale` is nil. Returns the locale name, or nil on failure.
---@param locale? string
---@param category? locale_category
---@return string? name
function os.setlocale(locale, category) end

---Returns the current time as a timestamp, or the timestamp described by the given date table.
---@param date? osdate
---@return integer timestamp
---@nodiscard
function os.time(date) end

---Returns a file name that can be used for a temporary file.
---@return string name
---@nodiscard
function os.tmpname() end
