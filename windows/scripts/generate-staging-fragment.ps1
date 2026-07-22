[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$StageDirectory,
    [Parameter(Mandatory = $true)]
    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-StagingIdentifier([string]$RelativePath) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($RelativePath.ToLowerInvariant())
    $digest = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes))
    return $digest.Substring(0, 24)
}

if (-not (Test-Path -LiteralPath $StageDirectory -PathType Container)) {
    throw "Staging directory is missing: $StageDirectory"
}

$settings = [Xml.XmlWriterSettings]::new()
$settings.Indent = $true
$settings.Encoding = [Text.UTF8Encoding]::new($false)
$writer = [Xml.XmlWriter]::Create($Destination, $settings)
try {
    $writer.WriteStartDocument()
    $writer.WriteStartElement('Wix', 'http://wixtoolset.org/schemas/v4/wxs')
    $writer.WriteStartElement('Fragment')
    $writer.WriteStartElement('ComponentGroup')
    $writer.WriteAttributeString('Id', 'SuiteFiles')

    $files = Get-ChildItem -LiteralPath $StageDirectory -File -Recurse | Sort-Object FullName
    foreach ($file in $files) {
        $relative = [IO.Path]::GetRelativePath($StageDirectory, $file.FullName).Replace('\', '/')
        if ($relative -ceq 'installer/vivido-windows-setup.exe') { continue }

        $parent = [IO.Path]::GetDirectoryName($relative)
        if ($null -eq $parent) { $parent = '' }
        $parent = $parent.Replace('\', '/')
        $directory = switch ($parent) {
            '' { 'INSTALLFOLDER' }
            'LICENSES' { 'LicensesFolder' }
            default { throw "Unexpected staging subdirectory: $parent" }
        }
        $identifier = Get-StagingIdentifier $relative

        $writer.WriteStartElement('Component')
        $writer.WriteAttributeString('Id', "StagedComponent_$identifier")
        $writer.WriteAttributeString('Directory', $directory)
        $writer.WriteAttributeString('Guid', '*')

        $writer.WriteStartElement('File')
        $writer.WriteAttributeString('Id', "StagedFile_$identifier")
        $writer.WriteAttributeString('Source', ('$(var.StageDir)\' + $relative.Replace('/', '\')))
        $writer.WriteAttributeString('Name', $file.Name)
        $writer.WriteEndElement()

        $writer.WriteStartElement('RegistryValue')
        $writer.WriteAttributeString('Root', 'HKCU')
        $writer.WriteAttributeString('Key', 'Software\Vivido\Suite\Components')
        $writer.WriteAttributeString('Name', $identifier)
        $writer.WriteAttributeString('Type', 'integer')
        $writer.WriteAttributeString('Value', '1')
        $writer.WriteAttributeString('KeyPath', 'yes')
        $writer.WriteEndElement()

        $writer.WriteEndElement()
    }

    $writer.WriteEndElement()
    $writer.WriteEndElement()
    $writer.WriteEndElement()
    $writer.WriteEndDocument()
} finally {
    $writer.Dispose()
}
